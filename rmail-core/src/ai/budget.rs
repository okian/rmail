//! The token/dollar budget enforcer: what may be spent, by whom, on which
//! model, before a request is allowed to leave the machine.
//!
//! # Spend is derived from the ledger, never accumulated beside it
//!
//! Every number this module compares against a cap comes from a `SUM` over
//! [`crate::ai::audit`]'s `ai_ledger` — the append-only audit trail. There is
//! deliberately no `ai_spend` counter table. A second accounting path would
//! be one dropped write, one crash between two transactions, or one call site
//! that forgot to increment away from disagreeing with the audit trail about
//! how much was spent, and it is the audit trail that has to be right: it is
//! the record of what actually left the machine. `ai_usage` (the day rollup
//! `record_call` maintains) is not used here either — not because it is
//! untrustworthy (it is written in the same transaction as the ledger insert)
//! but because it is keyed by day alone, with no account and no work-class
//! dimension, so it cannot answer the questions this module asks.
//!
//! The one thing the ledger could not previously say is *which budget* a call
//! was charged to, so `V27__ai_budget.sql` adds `ai_ledger.work_class` and
//! [`crate::ai::audit::record_call_charged`] writes it. Attribution lives with
//! the evidence, not in a side table that could drift from it.
//!
//! # Soft caps downgrade, hard caps block
//!
//! A **soft** cap means the work is still worth doing, just not at this price:
//! [`BudgetVerdict::Downgrade`] steps the requested model one rung down the
//! `opus → sonnet → haiku` ladder ([`ModelTier::downgrade`]) and the call
//! proceeds. A **hard** cap means it is not worth doing at all right now:
//! [`BudgetVerdict::Block`], and the provider is never called.
//!
//! Both boundaries are `>=`, matching [`crate::ai::queue::CostGate`]'s
//! existing `today_cost >= daily_cost_cap_usd`: spend *at* the cap has
//! consumed the cap. A budget of $5.00 permits spending up to but not
//! including $5.00; the micro-dollar exactly on the line is over.
//!
//! A soft cap is **one rung, not one rung per breached cap**. Breaching the
//! daily and monthly dollar soft caps simultaneously still downgrades opus to
//! sonnet, not to haiku. The ladder exists to make work cheaper, not to race
//! to the cheapest model the moment two caps happen to agree; the hard cap is
//! what stops work, and it is reached on its own schedule. If the requested
//! model is already on the bottom rung (or is not on the ladder at all) a soft
//! cap has nothing to downgrade *to* and resolves [`BudgetVerdict::Allow`] —
//! a soft cap never blocks, by definition.
//!
//! # The most restrictive answer wins
//!
//! A request is checked against several budgets at once — the global one, the
//! account's own, and (for bulk work) each of their bulk sub-budgets — across
//! two windows and two dimensions. Every check produces a [`Severity`], and
//! the verdict is the `max()` of them, exactly the deny-wins fold
//! [`crate::ai::policy`] applies across rules matching at one tier. A generous
//! per-account cap therefore cannot rescue a request from an exhausted global
//! cap: `Severity::Hard` from the global check outranks `Severity::Open` from
//! the account's, and the ordering on [`Severity`] is what makes that
//! structural rather than a property of the order the checks happen to run in.
//!
//! # Bulk work gets its own sub-budget, in one direction only
//!
//! Bulk work (a backlog walk — see [`WorkClass::for_priority`]) is charged
//! against **both** its scope's `all` budget and its `bulk` sub-budget.
//! Interactive work is charged against the `all` budget only. That asymmetry
//! is the whole point:
//!
//! - A backfill that exhausts its sub-budget stops, while the `all` budget
//!   still has room — so interactive and triage work keeps running. Bulk
//!   cannot starve the user.
//! - Interactive spend does not consume the bulk sub-budget, so a busy day of
//!   user-driven analysis leaves the backlog's reserved share intact. The
//!   user cannot starve the backlog either, short of exhausting the shared
//!   `all` budget — which is the ceiling both are supposed to share.
//!
//! With no explicit `bulk` row stored, the sub-budget is derived as
//! `ai.limits.budget.bulk_share` of the scope's effective `all` caps, so the
//! property holds out of the box rather than only once an operator configures
//! it.
//!
//! # A cap is a bound on what is *started*, not a hard ceiling on spend
//!
//! This is a check-then-act design and it cannot be anything else: the cost
//! of a call is not known until the response comes back, so the enforcer is
//! deciding whether to start a call whose price it can only guess at. Two
//! consequences, both deliberate:
//!
//! - **Concurrent checks can both pass.** Every evaluation reads spend that
//!   has already reached `ai_ledger`; a call in flight is invisible. What
//!   bounds the overshoot is therefore how many evaluations can be
//!   outstanding at once, which is why the live path evaluates *after*
//!   acquiring its `Semaphore(max_concurrency)` permit and its RPM token
//!   rather than before — see [`crate::ai::queue::AiWorkerPool::process_one`].
//!   Overshoot is bounded by `max_concurrency` calls, not by a whole
//!   dispatch cycle's worth of leases.
//! - **The batch path's overshoot is bounded differently**, because a batch's
//!   spend does not reach the ledger until its results are reconciled, up to
//!   24 hours later. [`crate::ai::queue::BatchCoordinator::maybe_submit`]
//!   refuses to submit a second batch of a pass while one is outstanding,
//!   which is what keeps that lag from compounding into an unbounded
//!   submission rate.
//!
//! Neither is a correctness bug to be fixed by locking: a transaction that
//! held a row lock across a network call to a model provider would serialize
//! the entire pipeline behind the slowest request, and it still could not
//! know the price in advance. The caps are sized as budgets, not as
//! guarantees, and the audit ledger — which records what was actually
//! spent — is the thing that is exact.
//!
//! # Money is compared in integer micro-dollars
//!
//! `ai_ledger.cost_usd` is `REAL` and stays that way — it is the historical
//! record, and this task does not get to rewrite it. But a *cap* is a number
//! a human typed (`5.00`), and `5.00` has no exact binary representation, so
//! comparing an accumulated `f64` sum against an `f64` cap can flip at the
//! boundary in whichever direction the last rounding error went. Caps are
//! therefore stored and compared as integer micro-dollars (1e-6 USD), and the
//! summed `cost_usd` is converted to micro-dollars **once**, at the
//! comparison boundary, by rounding half-away-from-zero. That is one rounding
//! step over a sum SQLite computed in double precision — not an accumulation
//! of them — and at micro-dollar granularity the double's relative error
//! (~1e-16) is many orders of magnitude below the least significant digit.
//!
//! # Windows are UTC calendar days and months
//!
//! Both windows are half-open `[start, end)` over `ai_ledger.created_at`, cut
//! on UTC calendar boundaries — the same boundaries
//! [`crate::ai::audit`]'s `day_key` uses, so a day this module counts is the
//! same day the ledger's own rollups and `mail ai cost` report. Yesterday's
//! spend therefore does not count against today's daily cap but does count
//! against this month's monthly cap, which is the entire reason both windows
//! exist.
//!
//! A UTC day is always wholly contained in its UTC month, which is what lets
//! [`spend_in_month`] answer both windows from a single scan: it sums the
//! month and, in the same pass, the subset of rows at or after the day's
//! start. Two range scans per budget would be correct too; one is correct and
//! half the work, and the containment that makes it valid is a property of
//! the calendar, not an assumption about the data.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate};
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;

use crate::config::{AiLimits, AiModelLadder};
use crate::error::Error;
use crate::storage::Database;

/// `account_id` of the global budget — the one every call counts toward,
/// whichever account it was made for.
///
/// A sentinel rather than `NULL` because it is half `ai_budgets`' primary
/// key and SQLite treats NULLs in a UNIQUE index as distinct from one
/// another, which would permit two conflicting "global" rows. `accounts.id`
/// is an autoincrementing `INTEGER PRIMARY KEY` that starts at 1 and is never
/// written explicitly, so `0` can never name a real account.
pub const GLOBAL_ACCOUNT_ID: i64 = 0;

/// Micro-dollars per dollar.
const MICROS_PER_USD: f64 = 1_000_000.0;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Which budget a call is charged to.
///
/// Stored in `ai_ledger.work_class` so spend can be attributed after the
/// fact — see the module docs on why attribution lives with the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkClass {
    /// Work a user is waiting on, or ordinary per-message pipeline work.
    /// Charged against the `all` budget only.
    Interactive,
    /// A backlog walk. Charged against the `all` budget *and* the `bulk`
    /// sub-budget — see the module docs on why that asymmetry is what keeps
    /// bulk from starving interactive work.
    Bulk,
}

impl WorkClass {
    /// The stable wire string stored in `ai_ledger.work_class`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Bulk => "bulk",
        }
    }

    /// Parse a wire string.
    ///
    /// # Errors
    /// [`Error::Internal`] for a value no version of this code wrote.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "bulk" => Ok(Self::Bulk),
            other => Err(Error::internal(format!(
                "unknown ai_ledger work_class: {other}"
            ))),
        }
    }

    /// Classify a queued job by its priority. At or beyond `bulk_priority`
    /// (`ai.limits.budget.bulk_priority`, [`crate::ai::queue::PRIORITY_BACKFILL`]
    /// by default) the job is a backlog walk and is charged as bulk.
    ///
    /// Priority is the right signal because it is what the *enqueuer* already
    /// had to decide: `AiDispatchLoop` enqueues freshly-synced mail at
    /// `PRIORITY_NORMAL`, an interactive re-analyze at `PRIORITY_RECENT`, and
    /// a backfill sweep at `PRIORITY_BACKFILL`. Deriving bulk-ness from it
    /// means no caller has to remember to classify itself a second time, and
    /// there is no way for the two answers to disagree.
    #[must_use]
    pub fn for_priority(priority: i64, bulk_priority: i64) -> Self {
        if priority >= bulk_priority {
            Self::Bulk
        } else {
            Self::Interactive
        }
    }
}

/// Which budget row a cap set belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BudgetClass {
    /// Every call in this scope counts against it.
    All,
    /// Only [`WorkClass::Bulk`] calls count against it.
    Bulk,
}

impl BudgetClass {
    /// The stable wire string stored in `ai_budgets.class`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Bulk => "bulk",
        }
    }

    /// Parse a wire string.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] — unlike [`WorkClass::parse`] this value can
    /// arrive straight from a client (`SetBudget`), so a bad one is the
    /// caller's argument being wrong, not this database holding a value no
    /// version of this code wrote.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "all" => Ok(Self::All),
            "bulk" => Ok(Self::Bulk),
            other => Err(Error::invalid_argument(format!(
                "unknown budget class {other:?}; expected \"all\" or \"bulk\""
            ))),
        }
    }

    /// The work classes charged against this budget, as the wire value
    /// [`spend_in_month`] filters on. `None` means "every class."
    fn work_filter(self) -> Option<WorkClass> {
        match self {
            Self::All => None,
            Self::Bulk => Some(WorkClass::Bulk),
        }
    }
}

/// A rung of the downgrade ladder.
///
/// Ordered cheapest-first so a `min` picks the more restrictive of two
/// ceilings without a second comparison helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// Bottom rung.
    Haiku,
    /// Middle rung.
    Sonnet,
    /// Top rung.
    Opus,
}

impl ModelTier {
    /// Classify a model id by family.
    ///
    /// Matched on the family substring rather than an exact id list on
    /// purpose: `claude-opus-4-8` and `claude-opus-5` are both opus and both
    /// need to downgrade to sonnet, and a table of exact ids would silently
    /// stop classifying — and therefore stop downgrading — the day Anthropic
    /// ships the next one. Returns `None` for anything that names no family,
    /// which a soft cap treats as "nothing to downgrade" rather than
    /// guessing.
    #[must_use]
    pub fn classify(model: &str) -> Option<Self> {
        let model = model.to_ascii_lowercase();
        if model.contains("opus") {
            Some(Self::Opus)
        } else if model.contains("sonnet") {
            Some(Self::Sonnet)
        } else if model.contains("haiku") {
            Some(Self::Haiku)
        } else {
            None
        }
    }

    /// The next rung down, or `None` at the bottom.
    #[must_use]
    pub fn downgrade(self) -> Option<Self> {
        match self {
            Self::Opus => Some(Self::Sonnet),
            Self::Sonnet => Some(Self::Haiku),
            Self::Haiku => None,
        }
    }

    /// The configured model id for this rung.
    #[must_use]
    pub fn model_id(self, ladder: &AiModelLadder) -> String {
        match self {
            Self::Opus => ladder.opus.clone(),
            Self::Sonnet => ladder.sonnet.clone(),
            Self::Haiku => ladder.haiku.clone(),
        }
    }
}

/// How badly one check was breached. Ordered least- to most-restrictive so
/// the fold across every applicable budget is `max()` — see the module docs'
/// "The most restrictive answer wins."
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Under every cap.
    Open,
    /// At or over a soft cap.
    Soft,
    /// At or over a hard cap.
    Hard,
}

/// One window's caps. `None` on a dimension means no cap on that dimension —
/// never zero, which is a real (and total) cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowCaps {
    /// Downgrade the model at or above this many micro-dollars.
    pub soft_usd_micros: Option<i64>,
    /// Block at or above this many micro-dollars.
    pub hard_usd_micros: Option<i64>,
    /// Downgrade the model at or above this many tokens.
    pub soft_tokens: Option<i64>,
    /// Block at or above this many tokens.
    pub hard_tokens: Option<i64>,
}

impl WindowCaps {
    /// Whether any dimension is capped at all. A budget with none is never
    /// worth querying spend for.
    #[must_use]
    pub fn is_unlimited(&self) -> bool {
        self.soft_usd_micros.is_none()
            && self.hard_usd_micros.is_none()
            && self.soft_tokens.is_none()
            && self.hard_tokens.is_none()
    }

    /// Fill each unset soft cap from its hard cap times `ratio`.
    ///
    /// A ratio outside `0.0..1.0` leaves the soft caps unset: a "soft cap" at
    /// or above the hard cap could never fire before the block does, and one
    /// at or below zero would downgrade every call from the first token,
    /// neither of which is a soft cap in any useful sense. An explicit soft
    /// cap stored by `SetBudget` is never overwritten.
    fn with_derived_soft(mut self, ratio: f64) -> Self {
        if !(ratio > 0.0 && ratio < 1.0) {
            return self;
        }
        if self.soft_usd_micros.is_none() {
            self.soft_usd_micros = self.hard_usd_micros.map(|hard| scale(hard, ratio));
        }
        if self.soft_tokens.is_none() {
            self.soft_tokens = self.hard_tokens.map(|hard| scale(hard, ratio));
        }
        self
    }

    /// This cap set scaled to `share` of itself — how a bulk sub-budget is
    /// derived from its scope's `all` caps when no explicit `bulk` row exists.
    ///
    /// Out-of-range shares are clamped rather than ignored, and the direction
    /// matters. `share <= 0.0` scales to zero — which, with a `>=` boundary,
    /// blocks all bulk work. That is what an operator setting `bulk_share = 0`
    /// is asking for; returning the caps *unchanged* (as an "invalid input,
    /// leave it alone" branch would) grants bulk the entire parent budget,
    /// turning a request to stop backlog spend into permission to spend
    /// everything. `share > 1.0` clamps to `1.0`: a sub-budget larger than
    /// the budget it is carved out of is not a sub-budget, and the parent
    /// caps bound it anyway. A NaN share is treated as `0.0` by the same
    /// comparison, which fails closed.
    fn scaled(self, share: f64) -> Self {
        // `share.partial_cmp(&0.0) != Some(Greater)` rather than
        // `!(share > 0.0)`: the two are equivalent, but the explicit form
        // says that NaN is a case being handled (it compares as `None` and
        // takes this branch, failing closed) rather than an accident of
        // negating a partial order.
        if share.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return Self {
                soft_usd_micros: self.soft_usd_micros.map(|_| 0),
                hard_usd_micros: self.hard_usd_micros.map(|_| 0),
                soft_tokens: self.soft_tokens.map(|_| 0),
                hard_tokens: self.hard_tokens.map(|_| 0),
            };
        }
        let share = share.min(1.0);
        Self {
            soft_usd_micros: self.soft_usd_micros.map(|v| scale(v, share)),
            hard_usd_micros: self.hard_usd_micros.map(|v| scale(v, share)),
            soft_tokens: self.soft_tokens.map(|v| scale(v, share)),
            hard_tokens: self.hard_tokens.map(|v| scale(v, share)),
        }
    }

    /// Grade `spend` against this window, naming the first cap breached at
    /// the winning severity.
    fn grade(&self, spend: Spend, window: &'static str) -> (Severity, Option<String>) {
        let checks: [(Option<i64>, i64, Severity, &str); 4] = [
            (
                self.hard_usd_micros,
                spend.usd_micros,
                Severity::Hard,
                "usd",
            ),
            (self.hard_tokens, spend.tokens, Severity::Hard, "token"),
            (
                self.soft_usd_micros,
                spend.usd_micros,
                Severity::Soft,
                "usd",
            ),
            (self.soft_tokens, spend.tokens, Severity::Soft, "token"),
        ];
        for (cap, actual, severity, dimension) in checks {
            if let Some(cap) = cap {
                if actual >= cap {
                    let kind = match severity {
                        Severity::Hard => "hard",
                        _ => "soft",
                    };
                    return (
                        severity,
                        Some(format!(
                            "{window} {dimension} {kind} cap reached ({actual} of {cap})"
                        )),
                    );
                }
            }
        }
        (Severity::Open, None)
    }
}

/// A stored or derived budget: both windows' caps for one `(scope, class)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetCaps {
    /// Caps over the current UTC calendar day.
    pub daily: WindowCaps,
    /// Caps over the current UTC calendar month.
    pub monthly: WindowCaps,
}

impl BudgetCaps {
    /// Whether both windows are entirely uncapped.
    #[must_use]
    pub fn is_unlimited(&self) -> bool {
        self.daily.is_unlimited() && self.monthly.is_unlimited()
    }

    fn with_derived_soft(self, ratio: f64) -> Self {
        Self {
            daily: self.daily.with_derived_soft(ratio),
            monthly: self.monthly.with_derived_soft(ratio),
        }
    }

    fn scaled(self, share: f64) -> Self {
        Self {
            daily: self.daily.scaled(share),
            monthly: self.monthly.scaled(share),
        }
    }
}

/// A budget as stored (or as it would be stored): a scope, a class, and caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// [`GLOBAL_ACCOUNT_ID`] or an `accounts.id`.
    pub account_id: i64,
    /// Which calls this budget governs.
    pub class: BudgetClass,
    /// The caps themselves.
    pub caps: BudgetCaps,
}

/// Spend over one window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spend {
    /// Estimated cost, in micro-dollars.
    pub usd_micros: i64,
    /// Every token: input, output, cache write, and cache read.
    pub tokens: i64,
}

/// Spend over both windows, from one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowSpend {
    /// Spend so far today (UTC).
    pub daily: Spend,
    /// Spend so far this month (UTC).
    pub monthly: Spend,
}

/// What [`BudgetEnforcer::evaluate`] resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// Under every cap (or over only a soft cap with nothing left to
    /// downgrade to). Dispatch the request as built.
    Allow,
    /// A soft cap is reached. Dispatch, but on this model instead.
    Downgrade {
        /// The model id to use — one rung below what was requested.
        model: String,
        /// Which cap forced it, for logs and for `mail ai budget status`.
        reason: String,
    },
    /// A hard cap is reached. The provider must not be called.
    Block {
        /// Which cap forced it. Names the scope, window, dimension, and the
        /// figures — safe for logs and for an `admin`-scoped RPC, but see
        /// `rmaild::ai_service`'s own handling for why a caller holding only
        /// `ai.invoke` is told less than this.
        reason: String,
        /// Unix seconds at which the window that blocked this request rolls
        /// over — the end of the UTC day for a daily cap, of the UTC month
        /// for a monthly one.
        ///
        /// Carried on the verdict rather than recomputed by the caller so a
        /// queue can hold the job out of its candidate set until then
        /// ([`crate::ai::queue::AiQueue::defer`]) instead of re-leasing and
        /// re-checking it on every tick. If several windows are breached at
        /// once this is the earliest of them: retrying then may find the job
        /// still blocked and simply defer it again, which is self-correcting,
        /// where taking the latest could hold work back long after it could
        /// have run.
        retry_at: i64,
    },
}

/// One request, as the enforcer sees it.
#[derive(Debug, Clone)]
pub struct BudgetRequest<'a> {
    /// The account this call is made for, or [`GLOBAL_ACCOUNT_ID`] for a call
    /// tied to no account (only the global budget applies then).
    pub account_id: i64,
    /// The model the caller intends to use — the input to a downgrade.
    pub model: &'a str,
    /// Which budget this call is charged to.
    pub work_class: WorkClass,
    /// "Now", in unix seconds. A parameter rather than a `Utc::now()` call so
    /// window arithmetic can be tested at a chosen instant instead of only at
    /// whatever moment the suite happens to run.
    pub now: i64,
}

/// A full spend/cap readout for one scope — what `AiPolicyService.GetSpend`
/// and `mail ai budget status` render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendReport {
    /// [`GLOBAL_ACCOUNT_ID`] or an `accounts.id`.
    pub account_id: i64,
    /// The UTC day the daily figures cover, `"YYYY-MM-DD"`.
    pub day: String,
    /// The UTC month the monthly figures cover, `"YYYY-MM"`.
    pub month: String,
    /// Every call in this scope, against the `all` budget.
    pub all: ClassReport,
    /// Bulk calls in this scope, against the `bulk` sub-budget.
    pub bulk: ClassReport,
}

/// One class's slice of a [`SpendReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassReport {
    /// Spend charged to this class over both windows.
    pub spend: WindowSpend,
    /// The caps actually in force — stored if an operator set them, derived
    /// otherwise.
    pub caps: BudgetCaps,
    /// Whether an `ai_budgets` row backs these caps, as opposed to them being
    /// derived from `ai.limits`. `mail ai budget status` says so, since
    /// "unset" and "set to exactly the default" behave the same today but
    /// diverge the moment the configured default changes.
    pub stored: bool,
}

// ---------------------------------------------------------------------------
// Money and scaling
// ---------------------------------------------------------------------------

/// Convert dollars to integer micro-dollars, rounding half away from zero.
///
/// Saturating rather than wrapping: a cap so large it overflows `i64`
/// micro-dollars (about ±9.2 trillion dollars) is not a value this enforcer
/// should misrepresent as small, and a NaN — which `as` would silently turn
/// into 0, i.e. "block everything" — becomes "no meaningful cap" instead.
#[must_use]
pub fn usd_to_micros(usd: f64) -> i64 {
    if usd.is_nan() {
        return i64::MAX;
    }
    let scaled = (usd * MICROS_PER_USD).round();
    if scaled >= i64::MAX as f64 {
        i64::MAX
    } else if scaled <= i64::MIN as f64 {
        i64::MIN
    } else {
        // Guarded above: `scaled` is finite and inside i64's range.
        scaled as i64
    }
}

/// Convert integer micro-dollars back to dollars, for display and the wire.
#[must_use]
pub fn micros_to_usd(micros: i64) -> f64 {
    // i64 -> f64 is lossy above 2^53 micro-dollars (~9 billion dollars); a
    // display path is the right place to accept that, and `f64::from` does
    // not accept i64 at all.
    #[allow(clippy::cast_precision_loss)]
    {
        micros as f64 / MICROS_PER_USD
    }
}

/// `value * ratio`, floored and clamped to a non-negative `i64`.
fn scale(value: i64, ratio: f64) -> i64 {
    #[allow(clippy::cast_precision_loss)]
    let scaled = (value as f64 * ratio).floor();
    if scaled <= 0.0 {
        0
    } else if scaled >= i64::MAX as f64 {
        i64::MAX
    } else {
        // Guarded above: `scaled` is finite, positive, and inside i64's range.
        #[allow(clippy::cast_possible_truncation)]
        {
            scaled as i64
        }
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// A half-open `[start, end)` range of unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    start: i64,
    end: i64,
}

/// The UTC calendar day and month `now` falls in, plus their labels.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Windows {
    day: Window,
    month: Window,
    day_label: String,
    month_label: String,
}

/// Resolve `now` into its UTC calendar day and month.
///
/// Falls back to a window covering all of time, labelled from the epoch, only
/// if `now` is so far out of range that `chrono` cannot represent it as a
/// date — impossible for a real `Utc::now()` timestamp. That fallback is
/// deliberately the *widest* window rather than the narrowest: it counts every
/// row in the ledger against the caps, so an unrepresentable clock over-counts
/// spend and blocks, instead of counting nothing and letting every call
/// through.
fn windows_for(now: i64) -> Windows {
    let fallback = || Windows {
        day: Window {
            start: i64::MIN,
            end: i64::MAX,
        },
        month: Window {
            start: i64::MIN,
            end: i64::MAX,
        },
        day_label: "1970-01-01".to_owned(),
        month_label: "1970-01".to_owned(),
    };
    let Some(dt) = DateTime::from_timestamp(now, 0) else {
        return fallback();
    };
    let date = dt.date_naive();
    let next_month = if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year().saturating_add(1), 1, 1)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
    };
    let (Some(month_start), Some(month_end), Some(day_start)) = (
        NaiveDate::from_ymd_opt(date.year(), date.month(), 1),
        next_month,
        date.succ_opt(),
    ) else {
        return fallback();
    };
    Windows {
        day: Window {
            start: date.and_utc_midnight(),
            end: day_start.and_utc_midnight(),
        },
        month: Window {
            start: month_start.and_utc_midnight(),
            end: month_end.and_utc_midnight(),
        },
        day_label: date.format("%Y-%m-%d").to_string(),
        month_label: date.format("%Y-%m").to_string(),
    }
}

/// Midnight UTC on a date, as unix seconds.
trait UtcMidnight {
    fn and_utc_midnight(&self) -> i64;
}

impl UtcMidnight for NaiveDate {
    fn and_utc_midnight(&self) -> i64 {
        // `and_hms_opt(0, 0, 0)` is `None` only for an hour/minute/second out
        // of range, which these literals are not; the fallback keeps this
        // total without an `expect`.
        self.and_hms_opt(0, 0, 0)
            .map_or(0, |dt| dt.and_utc().timestamp())
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Sum spend over `month`, and over the sub-range at or after `day_start`, in
/// one scan.
///
/// `account_id` of `None` sums every account (including ledger rows with no
/// account at all — a call made outside any account still spends real money
/// and must count against the global budget). `work_class` of `None` sums
/// every class.
///
/// The `WHERE` clause is built rather than written with `?n IS NULL OR
/// column = ?n` placeholders, the same shape [`crate::ai::audit`]'s
/// `select_calls` uses for the same reason: SQLite cannot use an index for a
/// column sitting behind an `OR` on a bound parameter, so the placeholder
/// form would silently fall back to scanning the whole month for every
/// account on a per-account query. Omitting the clause entirely when there is
/// nothing to filter on lets `idx_ai_ledger_account_created` do its job.
///
/// # Errors
/// A mapped storage error.
async fn spend_in_month(
    db: &Database,
    month: Window,
    day: Window,
    account_id: Option<i64>,
    work_class: Option<WorkClass>,
) -> Result<WindowSpend, Error> {
    let (day_usd, day_tokens, month_usd, month_tokens) = db
        .read(move |conn| {
            let mut params: Vec<SqlValue> = vec![
                SqlValue::from(month.start),
                SqlValue::from(month.end),
                SqlValue::from(day.start),
                SqlValue::from(day.end),
            ];
            let mut filters = String::new();
            if let Some(account_id) = account_id {
                params.push(SqlValue::from(account_id));
                filters.push_str(&format!(" AND account_id = ?{}", params.len()));
            }
            if let Some(work_class) = work_class {
                params.push(SqlValue::from(work_class.as_str().to_owned()));
                filters.push_str(&format!(" AND work_class = ?{}", params.len()));
            }
            let sql = format!(
                "SELECT
                     COALESCE(SUM(CASE WHEN created_at >= ?3 AND created_at < ?4
                         THEN cost_usd ELSE 0 END), 0.0),
                     COALESCE(SUM(CASE WHEN created_at >= ?3 AND created_at < ?4 THEN
                         input_tokens + output_tokens
                         + cache_creation_input_tokens + cache_read_input_tokens
                     ELSE 0 END), 0),
                     COALESCE(SUM(cost_usd), 0.0),
                     COALESCE(SUM(
                         input_tokens + output_tokens
                         + cache_creation_input_tokens + cache_read_input_tokens
                     ), 0)
                 FROM ai_ledger
                 WHERE created_at >= ?1 AND created_at < ?2{filters}"
            );
            conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, f64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
        })
        .await
        .map_err(Error::from)?;

    Ok(WindowSpend {
        daily: Spend {
            usd_micros: usd_to_micros(day_usd),
            tokens: day_tokens,
        },
        monthly: Spend {
            usd_micros: usd_to_micros(month_usd),
            tokens: month_tokens,
        },
    })
}

/// Read one stored budget row, if an operator has set one.
///
/// # Errors
/// A mapped storage error.
pub async fn get_budget(
    db: &Database,
    account_id: i64,
    class: BudgetClass,
) -> Result<Option<Budget>, Error> {
    let class_wire = class.as_str().to_owned();
    let caps = db
        .read(move |conn| {
            conn.query_row(
                "SELECT daily_soft_usd_micros, daily_hard_usd_micros,
                        daily_soft_tokens, daily_hard_tokens,
                        monthly_soft_usd_micros, monthly_hard_usd_micros,
                        monthly_soft_tokens, monthly_hard_tokens
                 FROM ai_budgets WHERE account_id = ?1 AND class = ?2",
                rusqlite::params![account_id, class_wire],
                |row| {
                    Ok(BudgetCaps {
                        daily: WindowCaps {
                            soft_usd_micros: row.get(0)?,
                            hard_usd_micros: row.get(1)?,
                            soft_tokens: row.get(2)?,
                            hard_tokens: row.get(3)?,
                        },
                        monthly: WindowCaps {
                            soft_usd_micros: row.get(4)?,
                            hard_usd_micros: row.get(5)?,
                            soft_tokens: row.get(6)?,
                            hard_tokens: row.get(7)?,
                        },
                    })
                },
            )
            .optional()
        })
        .await
        .map_err(Error::from)?;

    Ok(caps.map(|caps| Budget {
        account_id,
        class,
        caps,
    }))
}

/// Every stored budget row, keyed by `(account_id, class)` — one query
/// instead of up to four, since the table is tiny (one row per account per
/// class, and most deployments have neither).
///
/// # Errors
/// A mapped storage error.
async fn all_budgets(db: &Database) -> Result<BTreeMap<(i64, BudgetClass), BudgetCaps>, Error> {
    let rows = db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT account_id, class,
                        daily_soft_usd_micros, daily_hard_usd_micros,
                        daily_soft_tokens, daily_hard_tokens,
                        monthly_soft_usd_micros, monthly_hard_usd_micros,
                        monthly_soft_tokens, monthly_hard_tokens
                 FROM ai_budgets",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        BudgetCaps {
                            daily: WindowCaps {
                                soft_usd_micros: row.get(2)?,
                                hard_usd_micros: row.get(3)?,
                                soft_tokens: row.get(4)?,
                                hard_tokens: row.get(5)?,
                            },
                            monthly: WindowCaps {
                                soft_usd_micros: row.get(6)?,
                                hard_usd_micros: row.get(7)?,
                                soft_tokens: row.get(8)?,
                                hard_tokens: row.get(9)?,
                            },
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(Error::from)?;

    let mut out = BTreeMap::new();
    for (account_id, class, caps) in rows {
        // `Internal`, not the `InvalidArgument` `BudgetClass::parse` returns:
        // that method exists for a value a *client* supplied, and this one
        // came out of a column with a `CHECK` constraint. A value here that
        // no version of this code wrote is a database-integrity fault, and
        // reporting it to a `GetSpend` caller as their argument being wrong
        // would send them looking in the wrong place.
        let class = BudgetClass::parse(&class).map_err(|_| {
            Error::internal(format!(
                "ai_budgets row for account {account_id} holds an unknown class"
            ))
        })?;
        out.insert((account_id, class), caps);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Store (or replace) one budget row — `AiPolicyService.SetBudget` /
/// `mail ai budget set`.
///
/// # Errors
/// [`Error::InvalidArgument`] for a negative cap or a soft cap that is not
/// below its hard cap (a soft cap at or above the hard one can never fire
/// before the block does, so accepting it would silently store a downgrade
/// that never happens). Otherwise a mapped storage error.
pub async fn set_budget(db: &Database, budget: &Budget) -> Result<(), Error> {
    validate_window(&budget.caps.daily, "daily")?;
    validate_window(&budget.caps.monthly, "monthly")?;
    if budget.caps.is_unlimited() {
        // A stored row *wins* over the configured fallback (see
        // `resolve_caps`), so an all-unset one would not mean "leave the
        // defaults alone" — it would silently delete the `ai.limits`-derived
        // global ceiling, and with it the bulk sub-budget derived from it,
        // with no way to put them back short of editing the database. A
        // caller that meant "no cap on this dimension" leaves that dimension
        // unset alongside one that is set; a caller that sent nothing at all
        // has almost certainly forgotten the caps.
        return Err(Error::invalid_argument(
            "a budget must set at least one cap; sending none would replace the configured \
             ceiling with an unlimited one rather than restore it"
                .to_owned(),
        ));
    }
    if budget.account_id < 0 {
        return Err(Error::invalid_argument(format!(
            "budget account_id must be {GLOBAL_ACCOUNT_ID} (global) or a real account id, got {}",
            budget.account_id
        )));
    }

    let account_id = budget.account_id;
    let class = budget.class.as_str().to_owned();
    let caps = budget.caps;
    let now = chrono::Utc::now().timestamp();

    db.write(move |conn| {
        conn.execute(
            "INSERT INTO ai_budgets (
                 account_id, class,
                 daily_soft_usd_micros, daily_hard_usd_micros,
                 daily_soft_tokens, daily_hard_tokens,
                 monthly_soft_usd_micros, monthly_hard_usd_micros,
                 monthly_soft_tokens, monthly_hard_tokens,
                 updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(account_id, class) DO UPDATE SET
                 daily_soft_usd_micros = excluded.daily_soft_usd_micros,
                 daily_hard_usd_micros = excluded.daily_hard_usd_micros,
                 daily_soft_tokens = excluded.daily_soft_tokens,
                 daily_hard_tokens = excluded.daily_hard_tokens,
                 monthly_soft_usd_micros = excluded.monthly_soft_usd_micros,
                 monthly_hard_usd_micros = excluded.monthly_hard_usd_micros,
                 monthly_soft_tokens = excluded.monthly_soft_tokens,
                 monthly_hard_tokens = excluded.monthly_hard_tokens,
                 updated_at = excluded.updated_at",
            rusqlite::params![
                account_id,
                class,
                caps.daily.soft_usd_micros,
                caps.daily.hard_usd_micros,
                caps.daily.soft_tokens,
                caps.daily.hard_tokens,
                caps.monthly.soft_usd_micros,
                caps.monthly.hard_usd_micros,
                caps.monthly.soft_tokens,
                caps.monthly.hard_tokens,
                now,
            ],
        )?;
        Ok(())
    })
    .await
    .map_err(Error::from)?;

    tracing::info!(
        account_id,
        class = budget.class.as_str(),
        "ai budget stored"
    );
    Ok(())
}

/// Reject caps a client could not have meant.
fn validate_window(caps: &WindowCaps, window: &str) -> Result<(), Error> {
    for (value, name) in [
        (caps.soft_usd_micros, "soft_usd"),
        (caps.hard_usd_micros, "hard_usd"),
        (caps.soft_tokens, "soft_tokens"),
        (caps.hard_tokens, "hard_tokens"),
    ] {
        if value.is_some_and(|v| v < 0) {
            return Err(Error::invalid_argument(format!(
                "{window} {name} cap must not be negative"
            )));
        }
    }
    for (soft, hard, dimension) in [
        (caps.soft_usd_micros, caps.hard_usd_micros, "usd"),
        (caps.soft_tokens, caps.hard_tokens, "token"),
    ] {
        if let (Some(soft), Some(hard)) = (soft, hard) {
            if soft >= hard {
                return Err(Error::invalid_argument(format!(
                    "{window} {dimension} soft cap ({soft}) must be below the hard cap ({hard}); \
                     a soft cap at or above the hard cap can never downgrade before the block"
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The enforcer
// ---------------------------------------------------------------------------

/// Applies the stored and configured budgets to one request, before dispatch.
///
/// Constructed per evaluation, exactly as [`crate::ai::queue::CostGate`] is:
/// it holds no state of its own, and reading the budgets fresh is what makes
/// a `SetBudget` take effect on the very next job rather than at the next
/// daemon restart.
#[derive(Debug)]
pub struct BudgetEnforcer<'a> {
    /// The database `ai_ledger` and `ai_budgets` live in.
    pub db: &'a Database,
    /// The configured caps and enforcer knobs.
    pub limits: &'a AiLimits,
}

impl BudgetEnforcer<'_> {
    /// Decide what `request` may do — the check that must happen before the
    /// provider is called.
    ///
    /// # Errors
    /// A mapped storage error. A caller that cannot read the budgets must not
    /// treat that as permission to dispatch: the queue's worker retries the
    /// job rather than calling the provider, matching every other
    /// storage-failure path in that pipeline.
    #[tracing::instrument(
        skip(self),
        fields(
            account_id = request.account_id,
            model = request.model,
            work_class = request.work_class.as_str(),
        )
    )]
    pub async fn evaluate(&self, request: &BudgetRequest<'_>) -> Result<BudgetVerdict, Error> {
        let config = &self.limits.budget;
        if !config.enabled {
            return Ok(BudgetVerdict::Allow);
        }

        let windows = windows_for(request.now);
        let stored = all_budgets(self.db).await?;

        let mut severity = Severity::Open;
        let mut reason: Option<String> = None;
        // When the window that produced the winning severity rolls over.
        // Defaults to the end of the day: with no breach at all it is never
        // read, and a caller deferring on it would simply re-check sooner
        // than needed rather than later.
        let mut retry_at = windows.day.end;

        for (account_id, class) in self.applicable_scopes(request) {
            let caps = resolve_caps(&stored, account_id, class, self.limits);
            if caps.is_unlimited() {
                // Nothing to compare against; skip the scan entirely rather
                // than paying for a `SUM` whose answer cannot change the
                // verdict.
                continue;
            }
            let spend = spend_in_month(
                self.db,
                windows.month,
                windows.day,
                scope_filter(account_id),
                class.work_filter(),
            )
            .await?;

            let scope = describe_scope(account_id, class);
            for (window_caps, actual, label, window_end) in [
                (caps.daily, spend.daily, "daily", windows.day.end),
                (caps.monthly, spend.monthly, "monthly", windows.month.end),
            ] {
                let (graded, detail) = window_caps.grade(actual, label);
                if graded > severity {
                    severity = graded;
                    reason = detail.map(|detail| format!("{scope}: {detail}"));
                    retry_at = window_end;
                } else if graded == severity && graded != Severity::Open {
                    // Same severity from another window: the job cannot run
                    // until *every* breached window clears, but retrying at
                    // the earliest is self-correcting (it defers again if
                    // still blocked) and never holds work back longer than
                    // it has to.
                    retry_at = retry_at.min(window_end);
                }
            }
        }

        Ok(self.verdict(severity, reason, retry_at, request))
    }

    /// Every `(account_id, class)` pair this request is charged against, in
    /// the order the module docs describe. The verdict does not depend on
    /// this order — [`Severity`]'s `max()` fold is what decides — but the
    /// *reason* attached to it names the first check that reached the winning
    /// severity, and global-before-account reads better in a log line.
    fn applicable_scopes(&self, request: &BudgetRequest<'_>) -> Vec<(i64, BudgetClass)> {
        let mut scopes = vec![(GLOBAL_ACCOUNT_ID, BudgetClass::All)];
        if request.account_id != GLOBAL_ACCOUNT_ID {
            scopes.push((request.account_id, BudgetClass::All));
        }
        if request.work_class == WorkClass::Bulk {
            scopes.push((GLOBAL_ACCOUNT_ID, BudgetClass::Bulk));
            if request.account_id != GLOBAL_ACCOUNT_ID {
                scopes.push((request.account_id, BudgetClass::Bulk));
            }
        }
        scopes
    }

    /// Turn the folded severity into the verdict a caller acts on.
    fn verdict(
        &self,
        severity: Severity,
        reason: Option<String>,
        retry_at: i64,
        request: &BudgetRequest<'_>,
    ) -> BudgetVerdict {
        let reason = reason.unwrap_or_else(|| "ai budget cap reached".to_owned());
        match severity {
            Severity::Open => BudgetVerdict::Allow,
            Severity::Hard => {
                tracing::info!(reason = %reason, retry_at, "ai budget hard cap: blocking dispatch");
                BudgetVerdict::Block { reason, retry_at }
            }
            Severity::Soft => {
                let ladder = &self.limits.budget.ladder;
                let Some(tier) = ModelTier::classify(request.model).and_then(ModelTier::downgrade)
                else {
                    tracing::info!(
                        reason = %reason,
                        "ai budget soft cap reached, but the requested model is already on the \
                         bottom rung (or is not on the ladder); dispatching unchanged"
                    );
                    return BudgetVerdict::Allow;
                };
                let model = tier.model_id(ladder);
                // A downgrade target the ledger cannot price would record
                // `cost_usd = 0.0` for every call made after the soft cap
                // engaged (see `audit::estimate_cost_usd`) — so crossing the
                // *soft* cap would make the *hard* cap unreachable, which is
                // the one failure mode this whole module exists to prevent.
                // `AiModelLadder`'s docs say an operator retargeting the
                // ladder must add the id to that table; this is what happens
                // if they do not, and it is deliberately "keep the model the
                // handler chose" rather than "spend blind".
                if !crate::ai::audit::is_priced(&model) {
                    tracing::warn!(
                        reason = %reason,
                        downgrade_target = %model,
                        "ai budget soft cap reached, but the configured ladder names a model \
                         `ai::estimate_cost_usd` cannot price; downgrading to it would record a \
                         cost of zero and make the hard cap unreachable, so the requested model \
                         is kept. Add the id to the pricing table or fix `ai.limits.budget.ladder`"
                    );
                    return BudgetVerdict::Allow;
                }
                tracing::info!(
                    reason = %reason,
                    downgraded_to = %model,
                    "ai budget soft cap: downgrading model"
                );
                BudgetVerdict::Downgrade { model, reason }
            }
        }
    }
}

/// The `account_id` filter for a spend query: the global scope counts every
/// account, so it filters on nothing.
fn scope_filter(account_id: i64) -> Option<i64> {
    if account_id == GLOBAL_ACCOUNT_ID {
        None
    } else {
        Some(account_id)
    }
}

/// A human label for a scope, for the reason string on a verdict.
fn describe_scope(account_id: i64, class: BudgetClass) -> String {
    if account_id == GLOBAL_ACCOUNT_ID {
        format!("global {} budget", class.as_str())
    } else {
        format!("account {account_id} {} budget", class.as_str())
    }
}

/// The caps in force for one `(scope, class)`, stored or derived.
///
/// Resolution order:
///
/// 1. An explicit `ai_budgets` row, with any unset soft cap filled in from
///    its hard cap times `soft_cap_ratio`.
/// 2. Otherwise, for the global `all` budget: the ceilings configured under
///    `ai.limits` (`daily_cost_cap_usd`, `daily_token_cap`,
///    `monthly_cost_cap_usd`). This is why an operator who never calls
///    `SetBudget` is still bounded, and why there is one configured global
///    ceiling rather than two competing ones.
/// 3. Otherwise, for a `bulk` sub-budget: `bulk_share` of whatever the same
///    scope's `all` budget resolved to, so the reservation exists without
///    configuration.
/// 4. Otherwise (a per-account `all` budget nobody has set): unlimited. An
///    account is bounded by the global budget until an operator gives it one
///    of its own.
fn resolve_caps(
    stored: &BTreeMap<(i64, BudgetClass), BudgetCaps>,
    account_id: i64,
    class: BudgetClass,
    limits: &AiLimits,
) -> BudgetCaps {
    let config = &limits.budget;
    if let Some(caps) = stored.get(&(account_id, class)) {
        return caps.with_derived_soft(config.soft_cap_ratio);
    }
    match class {
        BudgetClass::All => {
            if account_id == GLOBAL_ACCOUNT_ID {
                configured_global_caps(limits).with_derived_soft(config.soft_cap_ratio)
            } else {
                BudgetCaps::default()
            }
        }
        BudgetClass::Bulk => {
            let parent = resolve_caps(stored, account_id, BudgetClass::All, limits);
            parent.scaled(config.bulk_share)
        }
    }
}

/// The global `all` ceilings implied by `ai.limits`.
///
/// `monthly` has no token cap because `ai.limits` does not configure one —
/// synthesizing one from the daily cap would invent a ceiling the operator
/// never wrote down.
fn configured_global_caps(limits: &AiLimits) -> BudgetCaps {
    BudgetCaps {
        daily: WindowCaps {
            soft_usd_micros: None,
            hard_usd_micros: Some(usd_to_micros(limits.daily_cost_cap_usd)),
            soft_tokens: None,
            hard_tokens: Some(i64::try_from(limits.daily_token_cap).unwrap_or(i64::MAX)),
        },
        monthly: WindowCaps {
            soft_usd_micros: None,
            hard_usd_micros: Some(usd_to_micros(limits.monthly_cost_cap_usd)),
            soft_tokens: None,
            hard_tokens: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Spend and caps for one scope, both classes — `AiPolicyService.GetSpend`.
///
/// # Errors
/// A mapped storage error.
pub async fn spend_report(
    db: &Database,
    limits: &AiLimits,
    account_id: i64,
    now: i64,
) -> Result<SpendReport, Error> {
    let windows = windows_for(now);
    let stored = all_budgets(db).await?;
    let filter = scope_filter(account_id);

    let mut classes = Vec::with_capacity(2);
    for class in [BudgetClass::All, BudgetClass::Bulk] {
        let spend =
            spend_in_month(db, windows.month, windows.day, filter, class.work_filter()).await?;
        classes.push(ClassReport {
            spend,
            caps: resolve_caps(&stored, account_id, class, limits),
            stored: stored.contains_key(&(account_id, class)),
        });
    }
    let mut classes = classes.into_iter();
    let (Some(all), Some(bulk)) = (classes.next(), classes.next()) else {
        // Unreachable: the loop above pushes exactly two entries.
        return Err(Error::internal(
            "budget report is missing a class".to_owned(),
        ));
    };

    Ok(SpendReport {
        account_id,
        day: windows.day_label,
        month: windows.month_label,
        all,
        bulk,
    })
}

#[cfg(test)]
mod tests;
