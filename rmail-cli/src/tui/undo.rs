//! The undo stack — task 112, tui.md §2.2.8.
//!
//! # What this holds, and what it deliberately does not
//!
//! A session-local, LIFO stack of inverse operations: move-back, unflag/
//! reflag, untag, and `CancelScheduled` (canceling a scheduled send inside
//! its undo window). tui.md's own enumeration also names "restore-from-
//! trash" as a fifth kind — this codebase has no distinct trash mechanism
//! to hook (`Action::Delete` is a hard, irreversible IMAP expunge, not a
//! move to a special folder; see its own doc in `model.rs`), so "restore"
//! and "move-back" are the same [`Entry::Move`] here, not two types.
//!
//! **No redo.** Not merely unbound — [`Stack`] has no method that could
//! replay a popped entry's *forward* action, and no way to inspect an
//! entry without consuming it via [`Stack::pop`]. tui.md's own reason:
//! inverse-op redo over a drifting IMAP mailbox would lie — the mailbox a
//! redo would re-apply against is not the one the original action saw.
//! Enforcing this by omission (rather than by an unused-but-present method)
//! is deliberate: a future change cannot casually wire a redo key to
//! something that already exists here without first deciding to add it
//! back, which is exactly the friction tui.md's law wants.
//!
//! # Every entry carries its own idempotency key
//!
//! Minted fresh, once, at construction (`Entry::mv`/`flags`/`tag`/
//! `cancel_scheduled` — never build a variant with a hand-supplied key).
//! This is *not* the forward action's own key reused: `rmail-core`'s
//! `IdempotencyStore` documents keys as globally single-use, with the
//! method folded into the request hash rather than the key's identity, so
//! reusing one key for a forward move and its inverse move-back would fail
//! the inverse outright as an `ALREADY_EXISTS` payload conflict, not
//! protect it (`rmail-core/src/idempotency/mod.rs`, "Keys are globally
//! single-use"). What the key actually protects is narrower and simpler:
//! if the `Cmd` an undo reissues is ever sent to the daemon more than once
//! — this codebase has no mechanism that does that *today* for `Move`/
//! `SetFlags` specifically, but minting the key costs nothing and is what
//! makes "so a retried undo cannot double-apply" true the moment one
//! exists, rather than something a later change would also have to add —
//! every attempt carries the same key this entry minted, so the daemon's
//! replay fence recognizes a second attempt rather than applying the
//! inverse twice.
//!
//! Not every entry's key reaches the wire today. `Move` and `Flags` do —
//! `Cmd::Move`/`Cmd::SetFlags` carry an `idempotency_key` field threaded
//! through to `MoveRequest`/`SetFlagsRequest`. `Tag` does not:
//! `AddTagRequest`/`RemoveTagRequest` have no such field on the proto at
//! all, so `Entry::Tag`'s key is minted and carried for API uniformity but
//! has nowhere to go — a pre-existing daemon-side gap, not one this task
//! introduces or is positioned to close. `CancelScheduled` is unused by
//! this task either way (see below).
//!
//! # `CancelScheduled` is a shape, not yet a producer
//!
//! Nothing in this task pushes one. Task 146 ("undo-send status chip")
//! owns the whole scheduled-send-undo lifecycle end to end — arming it
//! when a send is scheduled, the status-bar chip, canceling with "absent
//! id = most recent cancelable", and reopening the composer with cursor
//! position intact — and does so by pushing and popping this same variant.
//! Building only half of that here (push without the chip, or with a
//! design task 146 might not want) would commit to choices better made
//! once the chip's own requirements are in hand. `Entry::cancel_scheduled`
//! exists now so task 146 has a stable shape to build against.
//!
//! # Where a push actually happens
//!
//! Not here — this module never touches [`Model`](super::model::Model) or
//! [`Cmd`](super::model::Cmd), the same discipline `tui::ledger`'s
//! [`Ledger`](super::ledger::Ledger) follows. `model.rs`'s `apply_effect`
//! pushes [`Entry::Move`]/[`Entry::Flags`] once a `Cmd::Move`/`Cmd::SetFlags`
//! is *confirmed* (not at dispatch time) — see its own comments for why:
//! this crate's mutations are confirm-then-apply, not optimistic, so
//! pushing at dispatch would leave a phantom entry behind a failed action,
//! and for a bulk action issuing one `Cmd` per row there is no single
//! "did the batch succeed" moment to hook instead. `Cmd::TagApply` streams
//! a per-row outcome that never reaches `Effect`, so its push travels a
//! separate path — `grpc.rs`'s `apply_tags` sends `Msg::TagApplied` per
//! successful row, and `model.rs`'s own handler for it pushes
//! [`Entry::Tag`]. Both paths mint the entry only once the daemon has
//! actually agreed the forward action happened.
//!
//! # A bulk action pushes one entry per message it actually changes
//!
//! Archiving a 40-message visual selection with one keypress pushes up to
//! 40 entries, and `u` undoes them one at a time — as many presses as
//! entries pushed, which for a plain move is the same as the selection's
//! size. Flag toggles and tag additions can push fewer: a mixed-state flag
//! selection normalizes to one intent, and any row already at that target
//! is a local no-op that pushes nothing (`model.rs`'s `toggle_flag`); a
//! redundant tag apply gets the same treatment from the daemon's own
//! answer (`AddTagResponse.applications.is_empty()`, `grpc.rs`). tui.md
//! §2.2.8 does not specify batch-undo semantics either way, and this
//! module makes the simpler choice deliberately rather than by omission:
//! each entry mirrors exactly one confirmed daemon-side mutation that
//! actually happened (`Effect::Removed`/`Effect::Flags`/one
//! `Msg::TagApplied`), and a single "undo the whole batch" entry would
//! need to track partial failure within the batch itself (a 40-message
//! archive where message 17 failed server-side has 39 real inverses and
//! one that does not exist) — real complexity a first version does not
//! need. Worth revisiting only if a future task's acceptance actually asks
//! for it.

#[cfg(test)]
mod tests;

/// One inverse operation. See the module doc for what each carries and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Move `message_id` back to `dest_mailbox_id` — the folder a forward
    /// move (an archive, or a picked-folder move) took it out of.
    Move {
        /// Which message.
        message_id: i64,
        /// Where to move it back to.
        dest_mailbox_id: i64,
        /// See the module doc's idempotency section.
        idempotency_key: String,
    },
    /// Restore `message_id`'s complete flag set to exactly `flags` — the
    /// set it held before a forward `SetFlags`.
    Flags {
        /// Which message.
        message_id: i64,
        /// The complete flag set to restore.
        flags: Vec<String>,
        /// See the module doc's idempotency section.
        idempotency_key: String,
    },
    /// Reverse a tag application on `message_id`. `remove` is the
    /// *inverse* direction: a forward `tag add` pushes `remove: true`
    /// (undoing it removes the tag); a forward `tag rm` pushes
    /// `remove: false` (undoing it re-adds the tag).
    Tag {
        /// Which message.
        message_id: i64,
        /// Which tag, by name — the only identifier `Cmd::TagApply` takes.
        name: String,
        /// The direction *this entry's own* reissue goes.
        remove: bool,
        /// Minted, but not carried onto the wire today — see the module
        /// doc's idempotency section.
        idempotency_key: String,
    },
    /// Cancel the scheduled send at `outbox_id`, within its undo window.
    /// See the module doc — nothing in this task constructs one; the
    /// constructor exists for task 146.
    CancelScheduled {
        /// Which outbox entry.
        outbox_id: i64,
        /// See the module doc's idempotency section.
        idempotency_key: String,
    },
}

impl Entry {
    /// A move-back to `dest_mailbox_id`, with a freshly minted key.
    #[must_use]
    pub fn mv(message_id: i64, dest_mailbox_id: i64) -> Self {
        Self::Move {
            message_id,
            dest_mailbox_id,
            idempotency_key: new_key(),
        }
    }

    /// A flag-set restore to exactly `flags`, with a freshly minted key.
    #[must_use]
    pub fn flags(message_id: i64, flags: Vec<String>) -> Self {
        Self::Flags {
            message_id,
            flags,
            idempotency_key: new_key(),
        }
    }

    /// A tag reversal. `remove` is this entry's *own* reissue direction —
    /// see the variant's own doc for which way that runs relative to the
    /// forward action.
    #[must_use]
    pub fn tag(message_id: i64, name: String, remove: bool) -> Self {
        Self::Tag {
            message_id,
            name,
            remove,
            idempotency_key: new_key(),
        }
    }

    /// A scheduled-send cancellation. See the module doc — unused by this
    /// task; kept for task 146.
    #[must_use]
    #[allow(dead_code)] // task 146 is the named consumer — see the module doc.
    pub fn cancel_scheduled(outbox_id: i64) -> Self {
        Self::CancelScheduled {
            outbox_id,
            idempotency_key: new_key(),
        }
    }
}

/// A UUID, per prd.md's own spec for these ("mutating RPCs carry
/// `idempotency_key` (UUID)") — see `rmail-core::idempotency`'s module doc.
fn new_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A session-local, LIFO stack of [`Entry`]. See the module doc for the
/// no-redo guarantee this type's own method set (not a runtime check)
/// enforces.
///
/// Bounded at [`MAX_ENTRIES`] — a bulk action (`model::MAX_BULK`, 100)
/// pushes one entry per message, and a long session doing that repeatedly
/// with no cap at all is exactly the unbounded-growth shape this module's
/// own `known` map avoids elsewhere in this codebase (`tui::ledger`).
/// [`Stack::push`] evicts the oldest entry (the *bottom*, not the top —
/// nothing here is ever popped from that end in normal use) once full,
/// rather than refusing the new one: a push always represents something
/// that already happened server-side, and refusing to remember it would
/// make the newest, most likely to actually get undone, action the one
/// this stack forgets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stack {
    entries: Vec<Entry>,
}

/// See [`Stack`]'s own doc for why this exists and why it evicts the
/// oldest entry rather than refusing a push.
const MAX_ENTRIES: usize = 200;

impl Stack {
    /// Push a new entry on top, evicting the oldest one first if this
    /// would grow past [`MAX_ENTRIES`].
    pub fn push(&mut self, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }

    /// Pop the most recent entry, or `None` on an empty stack — the only
    /// way to observe an entry at all, so nothing can inspect one without
    /// also consuming it.
    pub fn pop(&mut self) -> Option<Entry> {
        self.entries.pop()
    }

    /// Whether there is anything to undo, without consuming an entry to
    /// find out. `model.rs`'s own pop-and-issue path (`undo`/`undo_send`)
    /// checks by popping directly instead — this is for a caller that only
    /// needs to know the answer, e.g. a future keybar hint gating "u
    /// undo"'s own visibility on whether there is anything to show it for.
    #[allow(dead_code)] // no such caller exists yet — see above.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
