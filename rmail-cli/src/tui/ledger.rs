//! The client unread ledger — task 111, tui.md §2.2.4/law 6.
//!
//! # Why this is client-derived at all
//!
//! `SyncService.Status` (`FolderStatus`, wire-mapped by `wire::folder`) is the
//! one RPC that enumerates an account's mailboxes, and it carries
//! `message_count` — total messages — and nothing about how many are unread.
//! That is a documented daemon gap (tui.md §19, "`FolderStatus.unread` —
//! folder unread counts are client-derived"), not an oversight this module
//! works around quietly: there is no server number to defer to, so every
//! count this type produces is an honest estimate, and law 6 ("honesty over
//! polish", tui.md §1.1) is what [`Estimate`] exists to make impossible to
//! misrepresent — a caller cannot print a bare integer, only `Unknown` or
//! `Estimated(u64)`, so a rendering site cannot accidentally claim precision
//! this module does not have. [`Estimate::label`] is the one sanctioned way
//! to turn either variant into text.
//!
//! # Seed, then adjust — never the other way around
//!
//! tui.md §2.2.4: "estimates maintained from loaded rows + `WatchEvents`
//! deltas". [`Ledger::seed`] is the *only* way a folder leaves [`Unknown`] —
//! called once a `MailService.List` page for it has actually loaded
//! (`Msg::Messages`'s success arm). [`Ledger::apply`] silently ignores a
//! delta about any folder [`seed`](Ledger::seed) has not touched yet, rather
//! than materializing a partial estimate from a stray event. Two things
//! motivate that: tui.md's own wording for the alternative state is
//! "unvisited" (§5.3 sidebar tie-in: "unvisited folders show `•`, never a
//! number"), and a folder nobody has opened is unvisited regardless of
//! whether the daemon happened to push an event about it; and this task's
//! own acceptance orders the two phases explicitly — "from loaded rows on
//! first load, **then** adjusted incrementally" — deltas adjust an existing
//! estimate, they do not conjure one. A count built purely from events
//! observed (rather than from a real page load) would in fact be a *worse*
//! lie than `•`: it would look precise while counting "events seen", not
//! "messages unread" — one `NewMail` for a folder that actually holds four
//! thousand unread messages would render `~1`.
//!
//! # Why a per-message map, not a bare counter
//!
//! A `FlagChanged` event's payload is the message's *complete new flag set*
//! (`Cmd::SetFlags`'s own doc: "a wholesale replace"), not a delta of what
//! was added or removed. Knowing whether that flip **decrements** or
//! **increments** the estimate requires knowing what this ledger last
//! believed about that specific message — so each tracked folder keeps a
//! `message_id -> was this unread` map alongside its running count, updated
//! by every seed and every delta together, never one without the other. The
//! same map is what lets [`Delta::Deleted`] (and [`Delta::Moved`] — see
//! below) find the right sign to apply, and what makes a delta about an
//! unknown message a safe no-op instead of a guess: an absent entry means
//! "this ledger has no record either way", and the honest response to that
//! is to leave the estimate alone, not to assume read (which would silently
//! undercount) or unread (which would silently overcount).
//!
//! # `Moved` only ever touches the folder a message left
//!
//! A first draft of this module credited the destination folder too — wrong,
//! and not a rare edge case. `MailStore::move_message`'s own doc
//! (`rmail-core/src/mail/mod.rs`) is explicit: the local row is **deleted**
//! once the server confirms the move, and the destination folder discovers
//! the message **fresh, under its real (new) UID and message id** on its
//! next sync — emitting its *own* `NewMail`. A `Moved` event's `message_id`
//! is the *old*, now-gone id; crediting the destination under it would
//! record unread state that no later event could ever reference again (the
//! real copy has a different id) while the destination's genuine `NewMail`
//! credits the same message a second time under the id that actually
//! persists. So [`Delta::Moved`] carries only the source folder and the old
//! id, and [`Ledger::apply`] handles it identically to [`Delta::Deleted`]:
//! drop the record, decrement the source if it was unread, touch nothing
//! else. The destination's own later `Arrived` is the only honest credit.
//!
//! # `Arrived` assumes unread — and that assumption has a known blind spot
//!
//! `NewMail`'s own payload carries no flag data at all — there is no way to
//! check whether newly synced mail is unread from the event alone.
//! [`Delta::Arrived`] assumes it is, per this task's own acceptance wording
//! ("a message arriving unread increments"), which is the right call for
//! the case this feature exists to serve: a message landing in an
//! already-visited folder during an active session genuinely is unread the
//! overwhelming majority of the time. It is the *wrong* call for a
//! **backlog walk of a folder nobody currently has open** — an initial
//! sync or a catch-up walk-down inserts rows via the same `Change::Added`
//! path a single live arrival does (`rmail-core/src/sync/{full,delta}.rs`),
//! and most of what a walk discovers is mail someone already read, possibly
//! on another client, long before this one ever saw it. For the *open*
//! folder this self-heals every ~300&nbsp;ms: `Msg::Changed`'s existing
//! coalesced reload reseeds whichever folder has focus, and `seed`
//! overwrites whatever an intervening `Arrived` had guessed. A **backgrounded**
//! folder undergoing its own walk has no such correction until it is next
//! opened — its estimate can run high for the rest of the session. Every
//! row such a walk discovers is, by construction, a row this ledger has no
//! prior record of (`Change::Added` — `NewMail`'s daemon-side producer —
//! only fires for a freshly inserted row, and a row inserted after the
//! last `List` cannot be in the page that seeded `known`), so this blind
//! spot is not narrowed by anything in [`Ledger::apply`]; the "already
//! known" guard discussed in the "stale replay" section below defends a
//! *different* case — a **replay** of an event about a row that already
//! is in the loaded page — not this one. Closing
//! that gap for good needs either flag data on `NewMail`'s payload (a
//! daemon-side change) or gating on a folder's own sync-progress state
//! (`FolderStatus.full_sync_done`, `sync.proto`) to tell "steady-state
//! arrival" apart from "backlog walk". The field itself reaches the client
//! today — `wire::folder` just discards it — so *availability* is not what
//! rules this out; *staleness* does: `model.folders` (where it would have
//! to live) is only refreshed by `Cmd::LoadFolders`, on startup and account
//! switch, not on a cadence a live delta stream could check against. Gating
//! `Arrived` on a `full_sync_done` snapshot that can silently go stale
//! mid-session would trade today's bounded overcount for an unbounded
//! *undercount*: once the real walk finishes, this ledger would keep
//! believing it has not, and would ignore every genuine live arrival for
//! that folder for the rest of the session. Closing it properly needs a
//! folder-status refresh on the same cadence `Msg::Changed` already reloads
//! messages on — a real change to the folder pane's own lifecycle, out of
//! this task.
//! Documented here rather than silently accepted: an estimate that is
//! sometimes optimistic in a bounded, self-correcting way is still honest
//! *as an estimate*; a caller must not read `Estimated(n)` as "verified".
//!
//! # A stale replay cannot double up on a fresh seed — mostly
//!
//! `grpc.rs`'s `watch()` subscribes with `since_seq: 0` every time it is
//! (re-)issued — including when `:account use` switches to a different
//! account — which replays **everything still inside the daemon's
//! retention window** (days, not seconds; see `grpc.rs`'s own module doc)
//! before resuming the live tail. Applying that whole replay naively on top
//! of an already-accurate seed would double-count exactly the way an
//! unbounded backlog walk does. [`Ledger`] defends against this with a
//! watermark: it tracks the highest event `seq` it has ever processed
//! (`last_seq`) and stamps that onto each folder at [`seed`](Ledger::seed)
//! time as a floor — [`Ledger::apply`] ignores any delta at or below a
//! folder's floor, so a replayed event that predates the ledger's own most
//! recent knowledge of that folder cannot reapply. `use_account` clears
//! every folder on a switch (via [`reset`](Ledger::reset), not
//! `Ledger::default()`) but **keeps** `last_seq` — `events.seq` is one
//! `AUTOINCREMENT` sequence shared by every account, so a seq this ledger
//! has already processed is gone for good and safe to keep as a floor no
//! matter which account produces the replay.
//!
//! That said, read the guarantee precisely: `last_seq` is the highest seq
//! *this client has itself received and decoded*, not the daemon's true
//! global tip. `WatchEvents` filters server-side by account, so while
//! watching account A this ledger only ever learns A's own event seqs —
//! nothing tells it how far a quieter account B's history extends in the
//! same shared sequence. Two cases therefore still apply a replayed event
//! on top of an already-accurate seed rather than rejecting it:
//!
//! - **The very first subscription of a session.** `last_seq` starts at
//!   `0`, so the first-ever [`seed`](Ledger::seed) stamps a floor of `0`
//!   and the entire backlog replay that follows has `seq > 0` — the floor
//!   does not filter *anything*.
//! - **Switching to an account more recently active than the one just
//!   left.** `last_seq` only reflects the account being left; if the new
//!   account's own retained history reaches a higher seq than that (a
//!   plausible ordering — seq order is arrival order across every account,
//!   not per-account recency), the floor lets that portion of its replay
//!   through too.
//!
//! Both collapse to the same shape: a delta whose `seq` is *newer* than
//! whatever this ledger has itself observed is indistinguishable from a
//! genuinely new one and is applied. This is a defence, not a guarantee —
//! see the "`Arrived` assumes unread" section above for why that residual
//! case is bounded (self-corrects on the open folder's own next
//! `Msg::Changed` reseed, same as the backlog-walk case) rather than
//! unbounded, which is what makes it acceptable for an estimate rather than
//! something this module must close outright.
//!
//! `Arrived` gets an extra layer the other three kinds do not: since a
//! genuine second `NewMail` for the same `message_id` cannot happen while
//! this ledger still holds a record of it (see the arm's own comment), a
//! replayed `Arrived` for a row still present in `known` is caught
//! regardless of the floor. That is narrower than it sounds, and does
//! **not** make the floor redundant for `Arrived` — it only covers a
//! replay landing while the row is still in `known`. The floor is still
//! what stops, for example, a message that arrived and was later deleted
//! (removing its `known` entry) from having its original, long-superseded
//! `Arrived` replayed back in as a phantom credit; with no record left to
//! consult, only `seq <= floor` catches that one (`seeding_stamps_the_folder_with_the_ledgers_current_high_water_seq`
//! and `reseeding_raises_the_floor_again` in `ledger/tests.rs` pin exactly
//! this — an `Arrived` for an id absent from `known`, rejected only by the
//! floor). For `Flags`, `Moved` and `Deleted` there is no extra layer at
//! all: a message can legitimately be flagged, moved or deleted more than
//! once across a session, so "seen this id before" never means "this must
//! be a replay" the way it can for `Arrived`. For all four kinds, then, the
//! two residual cases above are real: a stale delta with `seq` above
//! whatever this ledger has itself observed can still land on top of an
//! accurate seed.
//!
//! # Where a [`Delta`] actually comes from, and why it is batched
//!
//! Nothing in this module talks to gRPC or JSON. `wire::ledger_delta`
//! decodes a wire `Event` into one, and `grpc.rs`'s `watch()` accumulates
//! `(seq, Delta)` pairs and forwards them as `Msg::LedgerDelta(Vec<SeqDelta>)`
//! on the same 300&nbsp;ms ticker `Msg::Changed` already uses — **not**
//! uncoalesced. An earlier draft sent one message per event immediately,
//! reasoning that applying a delta costs no RPC so it did not need the
//! protection the ticker gives `Msg::Changed` against a burst of redundant
//! *reloads*. That missed a different cost: `model/drive.rs`'s run loop
//! repaints after every single `Msg`, so one message per backlog event
//! meant one full frame build per historical row on every reconnect — on a
//! mailbox with a real backlog, thousands of paints between two keystrokes,
//! each one queued behind the last. Batching on the ticker bounds that to
//! one paint per window, the same guarantee `Msg::Changed` already gives,
//! and narrows (though per the section above does not eliminate) the
//! replay-vs-seed race by shrinking how many separately-ordered messages a
//! reseed can land in the middle of. The batch itself is bounded too —
//! `grpc.rs`'s `DELTA_BATCH` flushes early past a size threshold rather
//! than letting a large backlog replay grow one `Vec` for the whole
//! 300&nbsp;ms window with no ceiling but the backlog's own size.
//!
//! # What this estimate does not cover: anything outside the loaded page
//!
//! [`seed`](Ledger::seed) only ever sees what `MailService.List` actually
//! returned — `grpc.rs`'s own `PAGE_SIZE` (500, matching the server-side
//! cap in `mail.proto`) for a folder listing. For any folder holding more
//! than one page, [`Estimate::Estimated`] is honestly "unread among the
//! rows this ledger has loaded", not "unread in the folder" — a mailbox
//! with 900 real unread messages, only 500 of them in the most recently
//! loaded page, can render `~3` if the other 897 sit outside it. This is
//! not a bug this module can fix by reading more of the folder (that is
//! what "estimate, not exact count" already concedes); it is a fact about
//! what `Estimated(n)` promises that a caller must not lose sight of.
//! `Folder.message_count` is already on the client (`wire::folder`) and
//! could in principle flag when a folder is large enough that its estimate
//! is likely truncated — left to whichever task first renders this
//! estimate, since this module has no reason to reach for a `Folder` today.
//!
//! # What this module does not bound
//!
//! A folder's `known` map only grows — there is no eviction as messages
//! scroll out of a loaded page or age out of the watch stream's window,
//! short of a fresh [`seed`](Ledger::seed) replacing the whole entry
//! wholesale. Bounded in practice by how much *history* a single watch
//! subscription can replay before the ledger's floor mechanism starts
//! rejecting it, not by page size alone the way an earlier draft of this
//! doc claimed; worth revisiting only if a future task's acceptance posits
//! a synthetic-scale session the way task 123's does for search.
//!
//! # One more bounded gap: a delta racing an in-flight `List`
//!
//! [`seed`](Ledger::seed) stamps a folder's floor from `last_seq` at the
//! moment it *runs*, not from whatever moment the `MailService.List`
//! response it is built from actually reflects server-side. A delta that
//! lands while that request is still in flight is applied normally against
//! the *old* entry, then erased outright when the response arrives and
//! [`seed`](Ledger::seed) replaces the folder wholesale — and because the
//! new floor is `last_seq` as of *now*, that delta's `seq` is at or below
//! it, so it can never reapply even though the fresh page may genuinely
//! predate it. Unlike the other blind spots this doc names, this one is
//! not consistently one direction: losing an `Arrived` under-counts,
//! losing a `Flags`-to-read or a `Deleted` over-counts. The reload window
//! is small and it heals on the same `Msg::Changed` cadence as everything
//! else this doc names, which is the whole reason it earns only this short
//! mention.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use super::model::{MessageRow, SEEN};

/// What the ledger can say about one folder's unread count — see the module
/// doc's law-6 discussion. Never destructure this to get at a bare number
/// without handling [`Unknown`](Self::Unknown); [`label`](Self::label) is
/// the one sanctioned way to turn this into text, precisely so a call site
/// cannot accidentally print a raw integer and overclaim precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimate {
    /// This folder has not been [`seed`](Ledger::seed)ed this session —
    /// unvisited, in tui.md's own word. Render `•`, never a number.
    Unknown,
    /// The best current guess: a loaded page, adjusted by every
    /// [`Delta`] observed since. Render `~{n}`.
    Estimated(u64),
}

impl Estimate {
    /// `•` for [`Unknown`](Self::Unknown), `~{n}` for
    /// [`Estimated`](Self::Estimated) — tui.md's own two spellings, in one
    /// place, so no call site needs to know them independently.
    #[allow(dead_code)] // see `Estimate`'s own doc: task 122 is the named consumer.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Unknown => "•".to_owned(),
            Self::Estimated(n) => format!("~{n}"),
        }
    }
}

/// One `WatchEvents` frame, decoded down to what [`Ledger::apply`] needs.
/// See the module doc's "where a `Delta` actually comes from" section —
/// `wire::ledger_delta` is what builds one of these from the wire `Event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    /// A message arrived in `mailbox_id`. Assumed unread — see the module
    /// doc's own section on why, and its documented blind spot.
    Arrived {
        /// Which folder it landed in.
        mailbox_id: i64,
        /// The new message.
        message_id: i64,
    },
    /// `message_id`'s complete flag set is now exactly `flags` (a
    /// wholesale replace, matching `Cmd::SetFlags`'s own contract — this
    /// is not "these flags changed", it is "these are all the flags now").
    Flags {
        /// Which folder it is currently in.
        mailbox_id: i64,
        /// Which message.
        message_id: i64,
        /// Its complete new flag set.
        flags: Vec<String>,
    },
    /// `message_id` left `mailbox_id` for elsewhere. Carries only the
    /// source — see the module doc's own section on why the destination is
    /// never credited here.
    Moved {
        /// Which folder it left.
        mailbox_id: i64,
        /// The message that moved (its *old* id — gone the moment this
        /// event fires; the destination's copy has a different one).
        message_id: i64,
    },
    /// `message_id` is gone from `mailbox_id`.
    Deleted {
        /// Which folder it was removed from.
        mailbox_id: i64,
        /// The removed message.
        message_id: i64,
    },
}

/// One decoded delta paired with the daemon-assigned `seq` of the
/// `WatchEvents` frame it came from — see [`Ledger::apply`] and the module
/// doc's "a stale replay cannot double up on a fresh seed" section for why
/// the sequence travels with the delta instead of being discarded once
/// `wire::ledger_delta` has done its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqDelta {
    /// The event's position in the daemon's log.
    pub seq: i64,
    /// What it decoded to.
    pub delta: Delta,
}

/// One folder's tracked state. Private — [`Ledger`] is the only thing that
/// ever sees this, always behind [`Estimate`] on the read side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FolderLedger {
    /// The running estimate. Kept in lockstep with `known` by every
    /// mutation this module makes — never recomputed from `known` on
    /// read, so a caller checking [`Ledger::get`] on every keystroke never
    /// pays for an `O(known.len())` scan.
    unread: u64,
    /// `message_id -> whether this ledger currently believes it is
    /// unread`, for every message a seed or an `Arrived` delta has
    /// directly observed. See the module doc: an absent entry means "no
    /// record", handled as a no-op, not a guess.
    known: HashMap<i64, bool>,
    /// The ledger's own `last_seq` at the moment this folder was last
    /// [`seed`](Ledger::seed)ed. A delta at or below this predates
    /// everything this folder's current estimate already reflects.
    floor: i64,
}

/// Per-folder unread-count estimates for the running session. Embedded on
/// [`Model`](super::model::Model) as `model.ledger`, seeded from
/// `Msg::Messages` and adjusted from `Msg::LedgerDelta` — see the module
/// doc for the full lifecycle. Reset wholesale on every account switch
/// (`use_account` in `model.rs`) — see the module doc's replay section for
/// why carrying entries across a re-subscription is not safe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    folders: HashMap<i64, FolderLedger>,
    /// The highest delta `seq` ever passed to [`apply`](Self::apply),
    /// across every folder. What a fresh [`seed`](Self::seed) stamps onto
    /// the folder it seeds as that folder's new floor.
    last_seq: i64,
}

impl Ledger {
    /// The current estimate for `mailbox_id`. See [`Estimate`]'s own doc
    /// for why this is unconsumed outside tests today.
    #[allow(dead_code)]
    #[must_use]
    pub fn get(&self, mailbox_id: i64) -> Estimate {
        self.folders
            .get(&mailbox_id)
            .map_or(Estimate::Unknown, |folder| {
                Estimate::Estimated(folder.unread)
            })
    }

    /// Drop every folder's tracked state — every folder reads back
    /// [`Estimate::Unknown`] until it is [`seed`](Self::seed)ed again —
    /// while deliberately *keeping* `last_seq`. Used by `use_account` on an
    /// account switch instead of `Ledger::default()`: `events.seq` is one
    /// `AUTOINCREMENT` sequence shared by every account
    /// (`rmail-core/src/events/mod.rs`), so a seq this ledger has already
    /// processed can never be produced again by any subscription, this
    /// account's or another's — throwing that number away on every switch
    /// would rearm the floor at zero for no reason. See the module doc's
    /// replay section for what this does and does not close.
    pub fn reset(&mut self) {
        self.folders.clear();
    }

    /// Seed (or wholesale re-seed) `mailbox_id` from a freshly loaded page
    /// — the "on first load" half of this module's own lifecycle. Replaces
    /// whatever this folder held before entirely, rather than merging: a
    /// fresh `MailService.List` page is the authoritative current picture
    /// of what is loaded, and replacing on every re-visit is what heals any
    /// drift a missed or out-of-window delta would otherwise leave behind
    /// permanently. Stamps the folder's floor to this ledger's current
    /// `last_seq` — see the module doc's replay section.
    pub fn seed(&mut self, mailbox_id: i64, rows: &[MessageRow]) {
        let mut folder = FolderLedger {
            floor: self.last_seq,
            ..FolderLedger::default()
        };
        for row in rows {
            let unread = !row.has_flag(SEEN);
            folder.known.insert(row.id, unread);
            if unread {
                folder.unread += 1;
            }
        }
        self.folders.insert(mailbox_id, folder);
    }

    /// Apply one delta, adjusting the affected folder's running count and
    /// per-message record together. A no-op if `seq` is at or below the
    /// affected folder's floor (see the module doc's replay section), or
    /// for a folder this ledger has never [`seed`](Self::seed)ed — silence
    /// is the honest answer whenever this ledger cannot be sure applying
    /// the delta is correct. For `Flags`/`Moved`/`Deleted` that also covers
    /// a message with no existing record: nothing to adjust without one.
    /// `Arrived` is the one exception, and inverted — see its own arm's
    /// comment: it is credited *only* when there is no existing record,
    /// and is the no-op when there is one.
    pub fn apply(&mut self, seq: i64, delta: &Delta) {
        self.last_seq = self.last_seq.max(seq);
        match delta {
            Delta::Arrived {
                mailbox_id,
                message_id,
            } => {
                let Some(folder) = self.folders.get_mut(mailbox_id) else {
                    return;
                };
                if seq <= folder.floor {
                    return;
                }
                // Credited only the first time this ledger ever hears about
                // `message_id` — not merely idempotent against an exact
                // redelivery, but deaf to *any* second `Arrived` for a
                // message_id it already has a record for, including one
                // the seed recorded as read. That record is more
                // trustworthy than the arrival assumption: `NewMail`'s
                // daemon-side producer (`Change::Added`, gated on
                // `outcome.inserted` in `rmail-core/src/sync/{full,delta}.rs`)
                // fires only for a freshly inserted row — so while this
                // ledger still holds a record for `message_id`, a second
                // `Arrived` for it cannot be a distinct new message, only a
                // replay of the first one. (`messages.id` has no
                // `AUTOINCREMENT` and a hard-deleted row's id can be
                // reused, so this is a property of *this ledger's current
                // record*, not a lifetime-unique-id guarantee from the
                // schema — reusing an id requires the original row to have
                // been deleted first, which is exactly what removes this
                // ledger's record of it via the `Deleted` arm below.) This
                // is what closes the more damaging half of the "stale
                // replay" section's residual gap: a replayed `Arrived` for
                // a message the seed itself already saw can no longer flip
                // an accurate read record back to unread.
                if !folder.known.contains_key(message_id) {
                    folder.known.insert(*message_id, true);
                    folder.unread = folder.unread.saturating_add(1);
                }
            }
            Delta::Flags {
                mailbox_id,
                message_id,
                flags,
            } => {
                let Some(folder) = self.folders.get_mut(mailbox_id) else {
                    return;
                };
                if seq <= folder.floor {
                    return;
                }
                let Some(was_unread) = folder.known.get(message_id).copied() else {
                    return;
                };
                // Exact match, not case-insensitive: `MessageRow::has_flag`
                // (what `seed` itself uses) compares exactly, and diverging
                // from it here would be the same "two engines quietly
                // disagree" mistake task 110's review caught in
                // `filter.rs`'s tag matching.
                let is_unread = !flags.iter().any(|f| f == SEEN);
                if was_unread != is_unread {
                    folder.known.insert(*message_id, is_unread);
                    if is_unread {
                        folder.unread = folder.unread.saturating_add(1);
                    } else {
                        folder.unread = folder.unread.saturating_sub(1);
                    }
                }
            }
            // Identical handling: see the module doc's own section on why
            // a `Moved` event never touches the folder a message moved
            // *into* — only the destination's own later `Arrived` credits
            // it, under the id that actually persists.
            Delta::Moved {
                mailbox_id,
                message_id,
            }
            | Delta::Deleted {
                mailbox_id,
                message_id,
            } => {
                let Some(folder) = self.folders.get_mut(mailbox_id) else {
                    return;
                };
                if seq <= folder.floor {
                    return;
                }
                if folder.known.remove(message_id) == Some(true) {
                    folder.unread = folder.unread.saturating_sub(1);
                }
            }
        }
    }
}
