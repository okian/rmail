//! The fuzzy finder (prd.md III-1, task 59): one prompt that jumps to
//! anything — a message, a folder, a contact, a saved search, a tag, or a
//! command.
//!
//! # It is not search, and it does not reuse search's pipeline
//!
//! Full search (`query` → `retrieve` → `fuse` → `features` → `rank` →
//! `present`) *ranks by relevance* over message bodies, on a per-query
//! budget of ~150 ms, and answers "what is the best answer to this
//! question". The finder *jumps by name* over short heterogeneous labels, on
//! a per-**keystroke** budget of ~16 ms, and answers "which of the things I
//! already know about did I mean". They share no candidate set (a mailbox, a
//! contact and a keybinding are not rows in `messages`), no scorer (a
//! subsequence aligner is not BM25 + RRF + a cross-encoder), and no latency
//! budget. Nothing here calls into `retrieve` or `rank`, and nothing there
//! calls into this: they are two questions, not two settings of one.
//!
//! # The latency bound, stated once
//!
//! prd.md: "< 16 ms to first batch, < 50 ms full ranked on 100k+ entries."
//! Five things enforce it, and each is a named constant or config key rather
//! than a hope:
//!
//! 1. **No I/O on the query path at all.** [`Finder`] holds an
//!    `Arc<RwLock<FinderStore>>` and nothing else; [`store::FinderStore`]
//!    holds no `Database`. A per-keystroke query therefore *cannot* reach
//!    SQLite — not "does not", cannot. The database is touched only by
//!    [`index::FinderIndex`]'s drain, on its own timer, on its own task.
//! 2. **A bounded corpus.** [`store::Limits`] caps both the entry count and
//!    the bytes those entries occupy (prd.md's "< 25 MB for 100k messages"),
//!    measured rather than assumed.
//! 3. **An `O(1)` prefilter before the `O(query × candidate)` aligner.**
//!    Every entry carries a `u64` of the characters it contains; an entry
//!    that lacks one of the query's characters cannot be a subsequence match
//!    and is rejected with one AND. See [`fold::char_mask`].
//! 4. **A bounded aligner.** The query is capped at
//!    [`score::MAX_QUERY_CHARS`] and each candidate blob at
//!    [`score::MAX_MATCH_CHARS`], so one DP cell count has a fixed ceiling
//!    no mailbox contents can raise.
//! 5. **A bounded result set.** A top-K heap of at most `limit` entries,
//!    flushed as descending batches every [`BATCH_STRIDE`] candidates so the
//!    first paint does not wait for the last entry.
//!
//! The CPU-bound scan runs under `spawn_blocking` — the same rule
//! `rank::l2`'s cross-encoder follows — so a long scan cannot stall the
//! runtime, and it polls `cancel` every [`CANCEL_STRIDE`] entries so a query
//! superseded by the next keystroke stops rather than finishing work nobody
//! will read.
//!
//! # Batches are snapshots, not deltas
//!
//! Each [`Batch`] is the complete current top-K, in descending order; a
//! client renders the latest and discards the previous. The alternative —
//! sending only what is new since the last flush — is wrong for a bounded
//! heap, because an entry that qualified early can be evicted later, and a
//! client accumulating deltas would keep showing a result the server has
//! since rejected. Snapshots make a dropped or reordered batch harmless: the
//! one flagged [`Batch::complete`] is authoritative on its own.
//!
//! # What a client needs, and nothing more
//!
//! Task 85's TUI overlay drives this through `FinderService.Find` without
//! reaching into any of it. The seam is: [`Query`] parses the prompt string
//! (including its sigil) so the overlay does not re-implement the scope
//! grammar; [`Batch`]es arrive on a stream and are rendered as they land;
//! [`Match::positions`] are char offsets into [`Match::primary_text`] and are
//! the only thing a highlight renderer needs; and cancellation is "start the
//! next `Find`", which supersedes the previous one server-side. No part of
//! the overlay needs to know that a store, a fold, or a heap exists.

pub mod fold;
pub mod index;
pub mod rank;
pub mod score;
pub mod store;

#[cfg(test)]
mod tests;

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::ops::ControlFlow;
use std::sync::{Arc, PoisonError, RwLock};

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::config::{FinderConfig, FinderRanking, FinderScope};
use crate::error::Error;

use rank::Ranked;
use score::Scorer;
use store::{Entry, FinderStore};

/// How many entries the scan walks between cancellation checks.
///
/// Small enough that a superseded query stops well inside one frame (a
/// thousand mask tests is single-digit microseconds), large enough that the
/// atomic load does not show up next to the work it guards.
pub const CANCEL_STRIDE: usize = 1_024;

/// How many entries the scan walks between batch flushes — prd.md's "~every
/// 2k candidates".
pub const BATCH_STRIDE: usize = 2_048;

/// The most partial batches one query may flush before its final one.
///
/// Every flush materializes up to `limit` results, which means sorting the
/// heap and computing highlight positions for each — real work, done for a
/// frame the user may never see. Four is enough for a list that visibly
/// fills in and few enough that the intermediate renders cannot cost more
/// than the scan itself.
pub const MAX_INTERMEDIATE_BATCHES: usize = 4;

/// The longest prompt [`Query::parse`] will carry through to folding.
///
/// See [`clamp_input`]. Two orders of magnitude above anything a picker
/// prompt holds, and still four orders below what a proto `string` allows.
pub const MAX_QUERY_INPUT_CHARS: usize = 4_096;

// ---------------------------------------------------------------------------
// kinds
// ---------------------------------------------------------------------------

/// What sort of thing an index entry is.
///
/// Three numbering schemes meet here and none of them may be inferred from
/// another: the storage code (`finder_index.kind`, 0-based, prd.md's own
/// numbering), the wire enum (`rmail.v1.ItemKind`, 1-based because proto3
/// reserves 0 for `UNSPECIFIED`), and this type's array slot. [`ItemKind`]
/// owns the first and the third explicitly; the daemon owns the second.
/// Deriving any of them from the others is how an off-by-one becomes a
/// silently mis-typed result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    /// A message: subject, with sender as the secondary line.
    Message,
    /// A mailbox, by full path.
    Mailbox,
    /// A contact: display name, with the address as the secondary line.
    Contact,
    /// A saved search, by name.
    SavedSearch,
    /// A tag, by full hierarchical name.
    Tag,
    /// A command-palette action id.
    Command,
}

impl ItemKind {
    /// Every kind, in `finder_index.kind` order.
    pub const ALL: [ItemKind; 6] = [
        ItemKind::Message,
        ItemKind::Mailbox,
        ItemKind::Contact,
        ItemKind::SavedSearch,
        ItemKind::Tag,
        ItemKind::Command,
    ];

    /// How many kinds there are, for the store's per-kind array.
    pub const COUNT: usize = Self::ALL.len();

    /// The `finder_index.kind` code, per prd.md's schema comment.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::Message => 0,
            Self::Mailbox => 1,
            Self::Contact => 2,
            Self::SavedSearch => 3,
            Self::Tag => 4,
            Self::Command => 5,
        }
    }

    /// The kind with this storage code, or `None` for a code this build does
    /// not know — a row written by a newer version is skipped rather than
    /// guessed at.
    #[must_use]
    pub const fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Message),
            1 => Some(Self::Mailbox),
            2 => Some(Self::Contact),
            3 => Some(Self::SavedSearch),
            4 => Some(Self::Tag),
            5 => Some(Self::Command),
            _ => None,
        }
    }

    /// This kind's slot in the store's per-kind array.
    #[must_use]
    pub const fn slot(self) -> usize {
        // Deliberately the same as `code`, but spelled separately: `code` is
        // a persisted wire-ish number that must never change, and a slot is
        // an implementation detail of one array. Tying them together would
        // make reordering the array a schema migration.
        match self {
            Self::Message => 0,
            Self::Mailbox => 1,
            Self::Contact => 2,
            Self::SavedSearch => 3,
            Self::Tag => 4,
            Self::Command => 5,
        }
    }

    /// The lowercase name used on the command line (`--scope contacts` names
    /// a scope, not a kind, but `--json` prints kinds).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Mailbox => "mailbox",
            Self::Contact => "contact",
            Self::SavedSearch => "saved_search",
            Self::Tag => "tag",
            Self::Command => "command",
        }
    }
}

// ---------------------------------------------------------------------------
// scopes and sigils
// ---------------------------------------------------------------------------

/// Which kinds a query searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything.
    All,
    /// Exactly one kind.
    Only(ItemKind),
}

impl Scope {
    /// The kinds this scope walks.
    #[must_use]
    pub fn kinds(self) -> &'static [ItemKind] {
        match self {
            Self::All => &ItemKind::ALL,
            Self::Only(ItemKind::Message) => &[ItemKind::Message],
            Self::Only(ItemKind::Mailbox) => &[ItemKind::Mailbox],
            Self::Only(ItemKind::Contact) => &[ItemKind::Contact],
            Self::Only(ItemKind::SavedSearch) => &[ItemKind::SavedSearch],
            Self::Only(ItemKind::Tag) => &[ItemKind::Tag],
            Self::Only(ItemKind::Command) => &[ItemKind::Command],
        }
    }

    /// The scope named on a command line or in config.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "all" => Self::All,
            "messages" | "message" => Self::Only(ItemKind::Message),
            // Both spellings: prd.md's scope list says `mailboxes`, its
            // config key says `folders`, and a user who guesses either is
            // right.
            "mailboxes" | "mailbox" | "folders" | "folder" => Self::Only(ItemKind::Mailbox),
            "contacts" | "contact" => Self::Only(ItemKind::Contact),
            "searches" | "saved_searches" | "saved-searches" => Self::Only(ItemKind::SavedSearch),
            "tags" | "tag" => Self::Only(ItemKind::Tag),
            "commands" | "command" => Self::Only(ItemKind::Command),
            _ => return None,
        })
    }

    /// The name this scope is written with.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only(ItemKind::Message) => "messages",
            Self::Only(ItemKind::Mailbox) => "mailboxes",
            Self::Only(ItemKind::Contact) => "contacts",
            Self::Only(ItemKind::SavedSearch) => "searches",
            Self::Only(ItemKind::Tag) => "tags",
            Self::Only(ItemKind::Command) => "commands",
        }
    }
}

impl From<FinderScope> for Scope {
    fn from(scope: FinderScope) -> Self {
        match scope {
            FinderScope::All => Self::All,
            FinderScope::Messages => Self::Only(ItemKind::Message),
            FinderScope::Contacts => Self::Only(ItemKind::Contact),
            FinderScope::Folders => Self::Only(ItemKind::Mailbox),
            FinderScope::Tags => Self::Only(ItemKind::Tag),
            FinderScope::SavedSearches => Self::Only(ItemKind::SavedSearch),
            FinderScope::Commands => Self::Only(ItemKind::Command),
        }
    }
}

/// A prompt string, split into the scope its sigil selected and the text
/// left to match.
///
/// prd.md's sigil table, verbatim: `>` commands, `#` tags, `@` contacts, `/`
/// saved searches, `:` mailboxes, no sigil → the scope the finder was opened
/// in. The sigil is stripped before matching, which is the whole point:
/// `@ali` means "contacts, matching `ali`", not "anything containing `@ali`".
///
/// Parsed here rather than in each client so the CLI, the daemon and task
/// 85's overlay cannot disagree about what `>` means — the same argument
/// `search_cli` makes for leaving `~`/`=` to `query::parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The scope, after any sigil.
    pub scope: Scope,
    /// The text to match, sigil removed.
    pub text: String,
    /// Whether a sigil selected the scope (as opposed to inheriting it).
    pub sigil: bool,
}

impl Query {
    /// Split `input` into a scope and the text to match, starting from
    /// `default_scope` when no sigil is present.
    ///
    /// A lone sigil (`>` with nothing after it) selects the scope and leaves
    /// an empty query, which is the useful reading: `>` on its own should
    /// list every command, exactly as prd.md's "empty query → signal-ranked
    /// recents/frequent/all-commands" describes.
    #[must_use]
    pub fn parse(input: &str, default_scope: Scope) -> Self {
        let trimmed = input.trim_start();
        let mut chars = trimmed.chars();
        let scope = match chars.next() {
            Some('>') => Some(Scope::Only(ItemKind::Command)),
            Some('#') => Some(Scope::Only(ItemKind::Tag)),
            Some('@') => Some(Scope::Only(ItemKind::Contact)),
            Some('/') => Some(Scope::Only(ItemKind::SavedSearch)),
            Some(':') => Some(Scope::Only(ItemKind::Mailbox)),
            _ => None,
        };
        match scope {
            Some(scope) => Self {
                scope,
                text: clamp_input(chars.as_str().trim()),
                sigil: true,
            },
            None => Self {
                scope: default_scope,
                text: clamp_input(trimmed.trim_end()),
                sigil: false,
            },
        }
    }
}

/// Cut a prompt down to something a person could have typed.
///
/// [`score::MAX_QUERY_CHARS`] bounds the *matcher*, but it is applied after
/// folding, so an unbounded input is still an unbounded fold on every
/// keystroke — and `FindRequest.query` is a proto `string`, which means a
/// client can send megabytes of it. This is the bound on the input side, and
/// it is deliberately far above `MAX_QUERY_CHARS` rather than equal to it:
/// folding can *shrink* text (a run of combining marks folds away entirely),
/// so truncating to the matcher's own limit could discard characters that
/// would have survived into the needle.
fn clamp_input(text: &str) -> String {
    if text.chars().count() <= MAX_QUERY_INPUT_CHARS {
        return text.to_owned();
    }
    tracing::debug!(
        limit = MAX_QUERY_INPUT_CHARS,
        "a finder prompt was truncated; nothing a human types reaches this length"
    );
    text.chars().take(MAX_QUERY_INPUT_CHARS).collect()
}

// ---------------------------------------------------------------------------
// queries and results
// ---------------------------------------------------------------------------

/// One finder query, already scope-resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindQuery {
    /// The text to match, sigil already stripped. Empty means "rank by
    /// signals alone".
    pub text: String,
    /// Which kinds to walk.
    pub scope: Scope,
    /// Restrict to one account. `None` searches every account.
    pub account_id: Option<i64>,
    /// Restrict to one mailbox — prd.md's `in-folder` scope. Only messages
    /// carry a mailbox, so this also implies "messages only" for the kinds
    /// that cannot satisfy it.
    pub mailbox_id: Option<i64>,
    /// The most results to return, already clamped by the caller.
    pub limit: usize,
    /// Whether to compute highlight positions.
    pub with_positions: bool,
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// `finder_index.item_id`.
    pub item_id: i64,
    pub kind: ItemKind,
    /// The row id in the source table — what an action acts on.
    pub ref_id: i64,
    /// The account this item belongs to, or 0 for a kind that has none.
    pub account_id: i64,
    /// The mailbox, for messages; 0 otherwise.
    pub mailbox_id: i64,
    /// The blended score. Higher is better; comparable only within one query.
    pub score: f64,
    /// The text to render, original and unfolded.
    pub primary_text: String,
    /// The dimmer second line.
    pub secondary: String,
    /// Ascending, deduped **char** offsets into `primary_text` — never byte
    /// offsets, and never offsets into `secondary`. See [`score`].
    pub positions: Vec<u32>,
}

/// One flush of the top-K heap: the complete current best, descending.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// The current top-K, best first. Replaces whatever the client showed
    /// before; see the module docs on why these are snapshots.
    pub items: Vec<Match>,
    /// Whether the scan is finished and this batch is authoritative.
    pub complete: bool,
    /// Entries walked so far, for `IndexStatus`-style diagnostics and for
    /// tests that need to assert the prefilter did its job.
    pub stats: ScanStats,
}

/// What one scan actually cost. The prefilter's effect is only observable
/// through this, which is why it is carried on every batch rather than
/// logged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Entries walked, in scope and past the account/mailbox filters.
    pub scanned: u64,
    /// Entries the `u64` mask admitted, i.e. how many times the aligner ran.
    pub aligned: u64,
    /// Entries the aligner actually matched.
    pub matched: u64,
    /// Whether the scan stopped early because `cancel` fired.
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// the finder
// ---------------------------------------------------------------------------

/// A heap entry: prd.md's ranking key plus where to find the row it names.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    ranked: Ranked,
    kind: ItemKind,
    slot: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ranked.cmp(&other.ranked)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The query side of the finder: an in-memory store and the weights to blend
/// with.
///
/// Cheap to clone — the store is shared, not copied — so a handler can hold
/// one and hand clones to spawned tasks, the same shape every other service
/// in this workspace uses.
#[derive(Clone)]
pub struct Finder {
    store: Arc<RwLock<FinderStore>>,
    ranking: FinderRanking,
    max_results: usize,
}

impl Finder {
    /// Build a finder over an existing store.
    ///
    /// The store is a parameter rather than created here because
    /// [`index::FinderIndex`] writes to the same one: there is exactly one
    /// store per daemon, and a constructor that made its own would give the
    /// drain and the queries two different views of the mailbox.
    #[must_use]
    pub fn new(store: Arc<RwLock<FinderStore>>, config: &FinderConfig) -> Self {
        Self {
            store,
            ranking: config.ranking.clone(),
            max_results: config.max_results as usize,
        }
    }

    /// The store this finder reads, for a caller that needs to inspect it
    /// (`IndexStatus`) without going through a query.
    #[must_use]
    pub fn store(&self) -> &Arc<RwLock<FinderStore>> {
        &self.store
    }

    /// The configured result cap, which every request's `limit` is clamped
    /// to.
    #[must_use]
    pub fn max_results(&self) -> usize {
        self.max_results
    }

    /// Clamp a client-supplied limit. `0` means "the configured default",
    /// matching how every other paged RPC in this workspace reads a zero.
    #[must_use]
    pub fn clamp_limit(&self, limit: usize) -> usize {
        if limit == 0 {
            self.max_results
        } else {
            limit.min(self.max_results)
        }
    }

    /// Run `query` to completion and return the final ranked page.
    ///
    /// A cancelled query is not an error: it returns whatever the scan had
    /// admitted before it stopped, which for a superseded keystroke is a page
    /// nobody will look at anyway.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] only if the blocking scan itself fails to run.
    pub async fn find(
        &self,
        query: FindQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<Match>, Error> {
        self.spawn_scan(query, cancel, |batch, last: &mut Vec<Match>| {
            *last = batch.items;
            ControlFlow::Continue(())
        })
        .await
        .map(|(_, last)| last)
    }

    /// Run `query`, delivering each flush of the top-K to `tx` as it happens.
    ///
    /// Returns the scan's statistics. A closed receiver stops the scan — the
    /// client hung up, and there is nobody left to serve.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if the blocking scan fails to run.
    pub async fn find_batched(
        &self,
        query: FindQuery,
        cancel: CancellationToken,
        tx: tokio::sync::mpsc::Sender<Batch>,
    ) -> Result<ScanStats, Error> {
        self.spawn_scan(query, cancel, move |batch, _: &mut ()| {
            // `blocking_send` from inside `spawn_blocking` is the supported
            // pairing: it parks a blocking-pool thread, never a runtime
            // worker, and gives the scan real back-pressure when a slow
            // client stops reading.
            match tx.blocking_send(batch) {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            }
        })
        .await
        .map(|(stats, ())| stats)
    }

    /// Move the scan onto the blocking pool, accumulating into `T`.
    ///
    /// The scan is CPU-bound over up to `max_entries` rows and must not run
    /// on a runtime worker — the same rule `rank::l2`'s cross-encoder
    /// follows. `spawn_blocking` tasks cannot be aborted, which is why the
    /// scan polls `cancel` itself every [`CANCEL_STRIDE`] entries rather
    /// than relying on the caller dropping this future: a superseded query
    /// has to *stop*, not merely stop being awaited.
    async fn spawn_scan<T, F>(
        &self,
        query: FindQuery,
        cancel: CancellationToken,
        mut sink: F,
    ) -> Result<(ScanStats, T), Error>
    where
        T: Default + Send + 'static,
        F: FnMut(Batch, &mut T) -> ControlFlow<()> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        let ranking = self.ranking.clone();
        let now = Utc::now().timestamp();
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            // The read guard is held for the whole scan, which is what makes
            // `Candidate::slot` (an index into a per-kind vector) valid at
            // materialize time. The drain takes the write lock only to apply
            // an already-computed batch, so a scan delays it by microseconds,
            // not by a query.
            let guard = store.read().unwrap_or_else(PoisonError::into_inner);
            let mut accumulator = T::default();
            let mut stats = ScanStats::default();
            {
                let stats = &mut stats;
                let accumulator = &mut accumulator;
                let mut collect = |batch: Batch| {
                    *stats = batch.stats;
                    sink(batch, accumulator)
                };
                scan(&guard, &query, &ranking, now, &cancel, &mut collect);
            }
            (stats, accumulator)
        })
        .await
        .map_err(|error| Error::internal(format!("the finder scan task failed: {error}")))
    }
}

/// Walk the store and feed the sink.
///
/// Split out of [`Finder`] so it is a plain function over a borrowed store:
/// the tests drive it directly with a hand-built store and no runtime, which
/// is what makes the prefilter and batching assertions cheap enough to be
/// worth writing.
fn scan<F>(
    store: &FinderStore,
    query: &FindQuery,
    ranking: &FinderRanking,
    now: i64,
    cancel: &CancellationToken,
    sink: &mut F,
) where
    F: FnMut(Batch) -> ControlFlow<()>,
{
    let mut scorer = Scorer::new(&query.text);
    let query_chars = query.text.chars().count();
    let query_mask = scorer.as_ref().map_or(0, Scorer::mask);
    let limit = query.limit.max(1);

    let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(limit + 1);
    let mut stats = ScanStats::default();
    let mut since_flush = 0usize;
    let mut since_cancel_check = 0usize;
    let mut flushes = 0usize;
    let mut dirty = false;

    'kinds: for &kind in query.scope.kinds() {
        // `in-folder` is a message-only restriction: nothing else carries a
        // mailbox, so under a mailbox filter every other kind is skipped
        // whole rather than filtered row by row.
        if query.mailbox_id.is_some() && kind != ItemKind::Message {
            continue;
        }
        for (slot, entry) in store.entries(kind).iter().enumerate() {
            since_cancel_check += 1;
            if since_cancel_check >= CANCEL_STRIDE {
                since_cancel_check = 0;
                if cancel.is_cancelled() {
                    stats.cancelled = true;
                    break 'kinds;
                }
            }
            if !matches_filters(entry, query) {
                continue;
            }
            stats.scanned += 1;
            since_flush += 1;

            let fuzzy = match scorer.as_mut() {
                Some(scorer) => {
                    if !fold::mask_admits(entry.mask, query_mask) {
                        continue;
                    }
                    stats.aligned += 1;
                    match scorer.score(entry.blob()) {
                        Some(score) => score,
                        None => continue,
                    }
                }
                // An empty query matches everything, ranked by signals alone.
                None => 0,
            };
            stats.matched += 1;

            let candidate = Candidate {
                ranked: Ranked {
                    score: rank::blend(fuzzy, kind, &entry.signals(), query_chars, now, ranking),
                    fuzzy,
                    last_activity: entry.last_activity,
                    length: u32::try_from(entry.primary_text().chars().count()).unwrap_or(u32::MAX),
                    item_id: entry.item_id,
                },
                kind,
                slot,
            };
            if admit(&mut heap, candidate, limit) {
                dirty = true;
            }

            if since_flush >= BATCH_STRIDE {
                since_flush = 0;
                if dirty && flushes < MAX_INTERMEDIATE_BATCHES {
                    flushes += 1;
                    dirty = false;
                    let batch = materialize(store, &heap, scorer.as_mut(), query, stats, false);
                    if sink(batch).is_break() {
                        return;
                    }
                }
            }
        }
    }

    // The final batch always goes out, even when the scan was cancelled or
    // matched nothing: a client that never sees `complete` cannot tell an
    // empty result from a stream that is still running.
    let batch = materialize(store, &heap, scorer.as_mut(), query, stats, true);
    let _ = sink(batch);
}

/// Whether an entry passes the non-textual filters.
fn matches_filters(entry: &Entry, query: &FindQuery) -> bool {
    if let Some(account_id) = query.account_id {
        // A kind with no account (a command, a contact) is visible under
        // every account filter: refusing it would make
        // `mail find --account 1 ">arch"` silently return nothing, which is
        // not what the filter means.
        if entry.account_id != 0 && entry.account_id != account_id {
            return false;
        }
    }
    if let Some(mailbox_id) = query.mailbox_id {
        if entry.mailbox_id != mailbox_id {
            return false;
        }
    }
    true
}

/// Push `candidate` into the bounded heap, returning whether the top-K
/// changed.
///
/// The heap holds `Reverse`d candidates so its `peek`/`pop` is the *worst*
/// held candidate — the one an arrival has to beat.
fn admit(heap: &mut BinaryHeap<Reverse<Candidate>>, candidate: Candidate, limit: usize) -> bool {
    if heap.len() < limit {
        heap.push(Reverse(candidate));
        return true;
    }
    match heap.peek() {
        Some(Reverse(worst)) if candidate > *worst => {
            heap.pop();
            heap.push(Reverse(candidate));
            true
        }
        _ => false,
    }
}

/// Turn the heap into a descending page of [`Match`]es.
///
/// Highlight positions are computed here, for the at-most-`limit` entries
/// that made the cut, rather than during the scan — see [`score`]'s module
/// docs on why that split exists.
fn materialize(
    store: &FinderStore,
    heap: &BinaryHeap<Reverse<Candidate>>,
    mut scorer: Option<&mut Scorer>,
    query: &FindQuery,
    stats: ScanStats,
    complete: bool,
) -> Batch {
    let mut candidates: Vec<Candidate> = heap.iter().map(|Reverse(c)| *c).collect();
    // Descending: `Ranked`'s natural order is "best is greatest".
    candidates.sort_unstable_by(|a, b| b.cmp(a));

    let items = candidates
        .into_iter()
        .filter_map(|candidate| {
            let entry = store.entries(candidate.kind).get(candidate.slot)?;
            let positions = match (query.with_positions, scorer.as_mut()) {
                (true, Some(scorer)) => scorer.positions(
                    entry.blob(),
                    entry.primary_text(),
                    entry.primary_folded_len as usize,
                ),
                _ => Vec::new(),
            };
            Some(Match {
                item_id: entry.item_id,
                kind: entry.kind,
                ref_id: entry.ref_id,
                account_id: entry.account_id,
                mailbox_id: entry.mailbox_id,
                score: candidate.ranked.score,
                primary_text: entry.primary_text().to_owned(),
                secondary: entry.secondary().to_owned(),
                positions,
            })
        })
        .collect();

    Batch {
        items,
        complete,
        stats,
    }
}
