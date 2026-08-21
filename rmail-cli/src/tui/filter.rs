//! The client filter engine — `f` in the design (tui.md §10, task 110).
//!
//! Filter, Search and Finder are "one grammar, three engines" (§10's own
//! heading): all three read the same operator syntax, but the filter is the
//! one engine that never leaves the client. It narrows the **already-loaded**
//! rows of whichever card has focus, with zero RPCs and inside a single
//! frame — the opposite trade-off from `/` search, which pays a round trip
//! for the whole corpus, ranked. A filter that quietly fired a request per
//! keystroke would not be a different engine, it would be search wearing a
//! different key.
//!
//! # Reusing the grammar, not re-parsing it
//!
//! This module does not tokenize a raw string itself. It calls
//! [`rmail_core::query::parse::parse`] — the exact function `/` search and
//! every other query surface in this codebase already uses — and then
//! classifies the result into what a *client* predicate can evaluate versus
//! what it cannot. Two independent parsers for one grammar is exactly the
//! kind of drift tui.md §10's "one grammar" promise exists to rule out: a
//! typo class this parser fixed and a hand-rolled second one missed would
//! silently make the filter and the search box disagree about what
//! `from:acme` means.
//!
//! # Reject inline, do not partially apply
//!
//! §10: typing `before:2024` in the filter "renders it red with `use / for
//! that`". That is a statement about the *whole* input, not about dropping
//! the one clause the client cannot evaluate and silently running the rest —
//! a user who typed `from:acme before:2024` expecting both to narrow the
//! list would get a real answer to a different, weaker question if the
//! `before:` were quietly discarded. [`classify`] instead returns
//! [`Classification::Unsupported`] the moment it sees *any* operator or sigil
//! outside the safe subset, naming it so the caller can render the "use `/`
//! for that" hint precisely (`C-Enter` — task 141 — is the escalation path
//! out of it, not this module's job to offer).
//!
//! # What "loaded rows only" costs
//!
//! [`MessageRow`] never carries a body (list rows must not pull one across
//! the wire per row — see its own doc comment), so `body:` has no client-side
//! answer even in principle and is correctly unsupported rather than
//! evaluated against nothing. `tag:`/`has:tag`/`has:note`/`ai:` are
//! recognized as safe grammar — tui.md §10 names them explicitly — but
//! [`MessageRow::tags`], [`MessageRow::has_note`] and [`MessageRow::ai`] are
//! themselves a "declared shape a future task populates" (see their own doc
//! comments): nothing in `ListMessages` carries that data yet, so today every
//! loaded row answers "no" to them, same as a row that genuinely has no tag
//! would. [`Predicate::matches`] is written and tested as if the data were
//! present, so it starts working the moment something populates those
//! fields — nothing about *this* task is a stub — but a caller that only
//! calls `matches` cannot tell "no" from "unknown" apart, which is exactly
//! what [`Predicate::unloaded_data`] is for: check it alongside `matches`,
//! don't just trust an empty result as if every safe operator were on equal
//! footing today.
//!
//! `from`/`subject`/free text substring-match whatever
//! [`wire::message_row`](super::model::wire::message_row) put in the row,
//! placeholders included: a subjectless message already renders as
//! `NO_SUBJECT` (`"(no subject)"`) and a nameless sender as `"(unknown
//! sender)"` before either ever reaches this module, so `f subject:no` or
//! `f from:unknown` can match a row client-side today even though the
//! equivalent `/ subject:no` / `/ from:unknown` would not (the underlying
//! column is `NULL` there, not literal text) — a pre-existing
//! rendering-vs-storage gap this task did not introduce and does not
//! attempt to close.
//!
//! # Why this whole module is `#[allow(dead_code)]` for now
//!
//! Nothing outside this module and its own tests calls [`classify`] yet —
//! there is no `f` keypress or filter prompt wired into `update`/`view`
//! today; that wiring is task 141's job, per its own acceptance. Every
//! public item here is real, tested, production code the moment task 141
//! consumes it — the same "declared shape a named future task consumes"
//! pattern task 92 established and [`layout`](super::layout) already
//! carries for the identical reason. Delete this allow in task 141's diff.
#![allow(dead_code)]

#[cfg(test)]
mod tests;

use rmail_core::query::parse::{self, AiPredicate, HasTarget, IsFlag, Mode, Operator};

use super::model::{AiFacts, MessageRow, ANSWERED, FLAGGED, SEEN};

/// What a raw filter string classifies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Every operator and sigil in the input is client-evaluable.
    Supported(Predicate),
    /// The input used an operator or sigil the filter cannot evaluate
    /// client-side, named exactly as a user would type its key (`"before"`,
    /// `"note"`, `"~"`, `"="`, ...) — [`parse::render_operator`]'s own key
    /// half for a real operator, or the bare sigil character for `~`/`=`.
    /// When more than one clause would independently disqualify the input,
    /// [`classify`] reports whichever it reaches first in its own two-pass
    /// order (every filter, then every term, then every phrase) — not
    /// necessarily whichever reads first in `raw`: `~x before:2024` reports
    /// `"before"` even though `~` comes first in the string. Good enough for
    /// a bare "why is this red" hint; a caller wanting the exact offending
    /// token highlighted in place would need position information this type
    /// does not carry.
    Unsupported(String),
}

/// A compiled, client-evaluable filter: operator constraints and free-text
/// words/phrases, all conjoined. A filter narrows; it does not rank, so
/// every clause must match — including an empty one, which is why the empty
/// input classifies to `Supported` of an empty `Predicate` rather than
/// `Unsupported`: no operators and no free text is a no-op that matches
/// every row, the correct reading of an empty filter box.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Predicate {
    filters: Vec<SafeFilter>,
    free_text: Vec<FreeText>,
}

impl Predicate {
    /// Whether `row` satisfies every clause.
    ///
    /// Pure and honest *given what `row` carries* — a `tag:` clause against
    /// a row with no tag data really does not match. But "no data loaded"
    /// and "loaded, genuinely no match" are different facts a caller showing
    /// this to a user needs to be able to tell apart (tui.md's own law
    /// against inventing counts), which is exactly what
    /// [`unloaded_data`](Self::unloaded_data) is for — check it alongside
    /// this, don't just trust an empty result.
    #[must_use]
    pub fn matches(&self, row: &MessageRow) -> bool {
        self.filters.iter().all(|f| f.matches(row)) && self.free_text.iter().all(|t| t.matches(row))
    }

    /// Every distinct kind of currently-unloaded [`MessageRow`] data this
    /// predicate's clauses depend on, in first-encountered order — empty for
    /// a predicate that only touches `from`/`to`/`subject`/`is`/`has:
    /// attachment`/free text, the operators this build can answer for real
    /// today. That empty result means three different things depending on
    /// the clause, and all three are still honest: `is:pinned`/`is:muted`
    /// and a `has:` value outside the documented vocabulary (`has:calendar`)
    /// are absent not because their data is loaded but because they are
    /// `Never` on *both* sides (see [`matches_is`]/[`matches_has`]'s own doc
    /// comments — no pin/mute predicate and no such `has:` target exists
    /// server-side either, so there is nothing to call "unloaded", just a
    /// clause that never matches anywhere); free text is absent because it
    /// always checks *some* real data (subject/from/from_addr/to), even
    /// though that is narrower than a live search's free text, which also
    /// reaches body/notes/attachments — an empty `unloaded_data()` says
    /// "this predicate is not blind about the fields it does check", not "a
    /// live search could not have found more". A caller with a non-empty
    /// result here is holding a `matches`
    /// answer that is honest but not necessarily *complete*: `tag:work`
    /// against every currently-loaded row evaluates to "no" not because no
    /// row is tagged work, but because no row's tags are loaded yet (see
    /// [`MessageRow::tags`]'s own doc comment) — a caller presenting that as
    /// an ordinary zero-result filter would be the exact silent wrong-answer
    /// this module's own header exists to rule out for unsupported
    /// operators; this method is what lets it avoid making the identical
    /// mistake for supported ones whose backing data just is not here yet.
    #[must_use]
    pub fn unloaded_data(&self) -> Vec<UnloadedData> {
        let mut found = Vec::new();
        for filter in &self.filters {
            let kind = match &filter.op {
                SafeOperator::Tag(_) | SafeOperator::Has(HasTarget::Tag) => {
                    Some(UnloadedData::Tags)
                }
                SafeOperator::Has(HasTarget::Note) => Some(UnloadedData::Note),
                SafeOperator::Ai(_) => Some(UnloadedData::Ai),
                SafeOperator::From(_)
                | SafeOperator::To(_)
                | SafeOperator::Subject(_)
                | SafeOperator::Has(HasTarget::Attachment | HasTarget::Other(_))
                | SafeOperator::Is(_) => None,
            };
            if let Some(kind) = kind {
                if !found.contains(&kind) {
                    found.push(kind);
                }
            }
        }
        found
    }
}

/// One kind of [`MessageRow`] data a safe operator can name but this build
/// does not populate onto loaded rows yet. See
/// [`Predicate::unloaded_data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadedData {
    /// `tag:`/`has:tag` — [`MessageRow::tags`].
    Tags,
    /// `has:note` — [`MessageRow::has_note`].
    Note,
    /// `ai:*` — [`MessageRow::ai`].
    Ai,
}

/// Parse `raw` and classify it as client-evaluable or not.
#[must_use]
pub fn classify(raw: &str) -> Classification {
    let parsed = parse::parse(raw);

    let mut filters = Vec::with_capacity(parsed.filters.len());
    for filter in parsed.filters {
        match SafeOperator::from_operator(filter.op) {
            Ok(op) => filters.push(SafeFilter {
                op,
                negated: filter.negated,
            }),
            Err(op) => {
                let (key, _value) = parse::render_operator(&op);
                return Classification::Unsupported(key.to_owned());
            }
        }
    }

    let mut free_text = Vec::with_capacity(parsed.terms.len() + parsed.phrases.len());
    for term in parsed.terms {
        if let Some(sigil) = unsupported_sigil(term.mode) {
            return Classification::Unsupported(sigil.to_owned());
        }
        if let Some(key) = degraded_operator_key(&term) {
            return Classification::Unsupported(key.to_owned());
        }
        free_text.push(FreeText {
            text: term.text,
            negated: term.negated,
        });
    }
    for phrase in parsed.phrases {
        if let Some(sigil) = unsupported_sigil(phrase.mode) {
            return Classification::Unsupported(sigil.to_owned());
        }
        free_text.push(FreeText {
            text: phrase.text,
            negated: phrase.negated,
        });
    }

    Classification::Supported(Predicate { filters, free_text })
}

/// `~`/`=` force a retrieval mode search can honor and a client substring
/// match cannot — there is no "semantic" or "exact-lexical" reading of a
/// predicate over already-loaded rows, so both sigils are unsupported.
/// `Mode::Auto` (no sigil) is the only mode a filter can ever evaluate.
fn unsupported_sigil(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Auto => None,
        Mode::Semantic => Some("~"),
        Mode::Lexical => Some("="),
    }
}

/// `term.looked_like_operator` means the raw text is shaped like `key:value`
/// (a bare identifier followed by `:` and a non-empty value) but did not
/// resolve to a real [`Operator`] — either `key` is not registered at all
/// (ordinary free text that happens to contain a colon, e.g. `urgency:high`
/// or `10:30`), or it *is* registered but `value` failed that operator's own
/// shape check (`date:last-week` is not an `a..b` range; `larger:huge` is
/// not a size; `subject:"` unquotes to an empty value; `ai:priority>` is a
/// malformed comparison).
///
/// Only the first of those two registered-but-malformed cases is worth
/// stopping the whole input for, and only when `key` is *outside* the safe
/// seven ([`is_safe_operator_key`]): free-texting `date:last-week` would
/// silently drop a constraint this filter has no other way to express, so it
/// must classify `Unsupported` the same as a value that *did* parse —
/// otherwise whether `date:last-week` narrows the list (never, silently) or
/// reports itself as unsupported (only when `last-week` happens *not* to
/// parse as a date, an implementation detail no user could predict) would
/// depend on the one thing tui.md §10 says a user should never have to
/// guess. A *safe* key with a malformed value is the opposite case: had the
/// value parsed, this filter would have evaluated it as a real constraint,
/// so a malformed one quietly becoming an inert free-text search costs
/// nothing beyond what `/` search already does with the identical input —
/// reporting `subject:"` or `ai:priority>` as
/// `Unsupported("subject")`/`Unsupported("ai")` would tell someone
/// mid-typing a filter this engine fully supports to go use `/` instead,
/// which is worse than useless. That was this function's bug in an earlier
/// revision: it reported every registered key it saw, safe or not, so the
/// ordinary act of typing `subject:"quarterly...` one character at a time
/// flashed "use `/` for that" the instant the opening quote landed.
///
/// Still `None` for a genuinely mid-typed operator like `from:` (empty
/// value, no quote at all): `split_operator` never sets
/// `looked_like_operator` for that case (see [`parse`]'s own docs), so it
/// degrades to searching the literal text `"from:"` — the same transient,
/// self-correcting behavior `/` search already has for the identical
/// half-typed input, not something this module can or should special-case
/// out of a plain string snapshot.
fn degraded_operator_key(term: &parse::Term) -> Option<&'static str> {
    if !term.looked_like_operator {
        return None;
    }
    let (key, _value) = term.text.split_once(':')?;
    let key = key.to_ascii_lowercase();
    if is_safe_operator_key(&key) {
        return None;
    }
    parse::OPERATORS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(name, _)| *name)
}

/// The operator-name spelling of every [`SafeOperator`] variant. Must mirror
/// [`SafeOperator::from_operator`]'s `Ok` arms exactly — a dedicated test in
/// `tests` cross-checks this against every entry in [`parse::OPERATORS`] by
/// actually parsing each one, so the two cannot silently drift apart if
/// `SafeOperator` ever gains or loses a variant. Used by
/// [`degraded_operator_key`] to tell "this key is safe but its value didn't
/// parse" (degrade to free text) apart from "this key is outside the safe
/// subset entirely" (report `Unsupported`).
fn is_safe_operator_key(key: &str) -> bool {
    matches!(key, "from" | "to" | "subject" | "has" | "tag" | "is" | "ai")
}

/// The seven [`Operator`] variants tui.md §10 names as filter-safe — a
/// closed subset, not a wrapper around `Operator`, so every match against
/// this type is exhaustive with no catch-all arm that could silently accept
/// an operator [`classify`] should have rejected instead.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SafeOperator {
    From(String),
    To(String),
    Subject(String),
    Has(HasTarget),
    Tag(String),
    Is(IsFlag),
    Ai(AiPredicate),
}

impl SafeOperator {
    /// `Err(op)` — handing the operator straight back, not just discarding
    /// it — if `op` is outside the safe subset: every other branch of the
    /// full grammar (§9's operator list) that this build's filter cannot
    /// evaluate client-side. Returning the value rather than `Option` is
    /// what lets [`classify`] call [`parse::render_operator`] only on the
    /// rejection path instead of on every filter, safe or not.
    fn from_operator(op: Operator) -> Result<Self, Operator> {
        match op {
            Operator::From(v) => Ok(Self::From(v)),
            Operator::To(v) => Ok(Self::To(v)),
            Operator::Subject(v) => Ok(Self::Subject(v)),
            Operator::Has(target) => Ok(Self::Has(target)),
            Operator::Tag(v) => Ok(Self::Tag(v)),
            Operator::Is(flag) => Ok(Self::Is(flag)),
            Operator::Ai(predicate) => Ok(Self::Ai(predicate)),
            Operator::Cc(_)
            | Operator::Body(_)
            | Operator::Filename(_)
            | Operator::Larger(_)
            | Operator::Smaller(_)
            | Operator::Before(_)
            | Operator::After(_)
            | Operator::On(_)
            | Operator::DateRange(_, _)
            | Operator::Note(_)
            | Operator::In(_)
            | Operator::Account(_)
            | Operator::Thread(_) => Err(op),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SafeFilter {
    op: SafeOperator,
    negated: bool,
}

impl SafeFilter {
    fn matches(&self, row: &MessageRow) -> bool {
        let hit = match &self.op {
            SafeOperator::From(v) => {
                contains_ci(&row.from, v)
                    || row.from_addr.as_deref().is_some_and(|a| contains_ci(a, v))
            }
            SafeOperator::To(v) => row.to.as_deref().is_some_and(|t| contains_ci(t, v)),
            SafeOperator::Subject(v) => contains_ci(&row.subject, v),
            SafeOperator::Has(target) => matches_has(target, row),
            SafeOperator::Tag(v) => row.tags.iter().any(|t| tag_matches(t, v)),
            SafeOperator::Is(flag) => matches_is(flag, row),
            SafeOperator::Ai(predicate) => matches_ai(predicate, row.ai.as_ref()),
        };
        hit != self.negated
    }
}

/// A free-text word or quoted phrase — structurally identical once neither
/// carries a sigil, which is exactly why [`classify`] folds
/// `ParsedQuery::terms` and `ParsedQuery::phrases` into one `Vec` here: a
/// filter matches a phrase as an ordinary substring, the same as a word,
/// since it has no lexical index to offer proximity/adjacency against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FreeText {
    text: String,
    negated: bool,
}

impl FreeText {
    fn matches(&self, row: &MessageRow) -> bool {
        let hit = contains_ci(&row.subject, &self.text)
            || contains_ci(&row.from, &self.text)
            || row
                .from_addr
                .as_deref()
                .is_some_and(|a| contains_ci(a, &self.text))
            || row
                .to
                .as_deref()
                .is_some_and(|t| contains_ci(t, &self.text));
        hit != self.negated
    }
}

/// Case-insensitive (ASCII, matching [`rmail_core::query::parse`]'s own
/// `IsFlag`/`HasTarget` value normalization) substring match.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Mirrors `rmail_core::retrieve::filtermask::tag_predicate_sql` exactly: a
/// trailing `/*` requests the tag *and its descendants* (`tag:project/*`
/// matches `project`, `project/alpha`, `project/alpha/q3`, ...); anything
/// else — including a name that happens to contain a `/` with no trailing
/// `*` — matches that exact tag name only. Getting this backwards (matching
/// children without `/*`, and never matching them with it) is exactly the
/// "one grammar" drift this module's header exists to rule out: `tag:` and
/// `tag:x/*` would mean two different things depending on whether `/` or a
/// keystroke sent the query.
fn tag_matches(applied: &str, pattern: &str) -> bool {
    match pattern.strip_suffix("/*") {
        // `tag:/*` is degenerate (no tag is named the empty string) — an
        // unmatchable exact name, the same call `tag_predicate_sql` makes,
        // rather than a prefix so wide it would match every hierarchical tag.
        Some(prefix) if !prefix.is_empty() => {
            applied.eq_ignore_ascii_case(prefix) || {
                let applied = applied.to_ascii_lowercase();
                let child_prefix = format!("{}/", prefix.to_ascii_lowercase());
                applied.starts_with(&child_prefix)
            }
        }
        _ => applied.eq_ignore_ascii_case(pattern),
    }
}

fn matches_has(target: &HasTarget, row: &MessageRow) -> bool {
    match target {
        HasTarget::Attachment => row.has_attachments,
        HasTarget::Note => row.has_note,
        HasTarget::Tag => !row.tags.is_empty(),
        // A `has:` value outside the documented vocabulary — this build does
        // not know what it means, so it honestly matches nothing rather than
        // guessing. Mirrors `retrieve::filtermask`'s own `RawEffect::Never`
        // for the identical case server-side.
        HasTarget::Other(_) => false,
    }
}

/// Mirrors `rmail_core::retrieve::filtermask`'s `Operator::Is` compilation
/// exactly (including `Pinned`/`Muted` never matching — that predicate does
/// not exist server-side yet either, so this is parity, not a shortcut) so
/// the filter and a `/` search of `is:` never disagree about a flag.
fn matches_is(flag: &IsFlag, row: &MessageRow) -> bool {
    match flag {
        IsFlag::Unread => !row.has_flag(SEEN),
        IsFlag::Read => row.has_flag(SEEN),
        IsFlag::Flagged => row.has_flag(FLAGGED),
        IsFlag::Replied => row.has_flag(ANSWERED),
        IsFlag::Pinned | IsFlag::Muted => false,
        IsFlag::Other(value) => matches_other_flag(value, row),
    }
}

fn matches_other_flag(value: &str, row: &MessageRow) -> bool {
    let flag = match value.to_ascii_lowercase().as_str() {
        "draft" => "\\Draft",
        "deleted" => "\\Deleted",
        "recent" => "\\Recent",
        "answered" => ANSWERED,
        // An unrecognized value is read as a literal custom IMAP keyword,
        // exactly as typed — the same fallback `retrieve::filtermask` uses.
        _ => return row.has_flag(value),
    };
    row.has_flag(flag)
}

/// Mirrors `rmail_core::retrieve::filtermask`'s `ai_predicate_sql` exactly
/// (same recognized keys, same `needs-reply`/`needs_reply` spelling, same
/// priority ordering) — see [`priority_rank`].
fn matches_ai(predicate: &AiPredicate, ai: Option<&AiFacts>) -> bool {
    let Some(ai) = ai else {
        return false;
    };
    match predicate {
        AiPredicate::Flag(name) => {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "needs-reply" | "needs_reply"
            ) && ai.needs_reply == Some(true)
        }
        AiPredicate::Equals(key, value) => match key.to_ascii_lowercase().as_str() {
            "category" => ai
                .category
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(value)),
            "sentiment" => ai
                .sentiment
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(value)),
            "priority" => ai
                .priority
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(value)),
            _ => false,
        },
        AiPredicate::GreaterThan(key, value) => {
            key.eq_ignore_ascii_case("priority")
                && ai
                    .priority
                    .as_deref()
                    .and_then(priority_rank)
                    .zip(priority_rank(value))
                    .is_some_and(|(row_rank, threshold)| row_rank > threshold)
        }
    }
}

/// `low < normal < high < critical` — must stay identical to
/// `retrieve::filtermask::priority_rank` (private to `rmail-core`, so it
/// cannot be shared across the crate boundary; [`tests`] pins all four
/// levels so the two cannot silently drift apart) for every value triage
/// actually writes. One narrow divergence exists beyond that: the server's
/// rank is a SQL `CASE` over a `BINARY`-collated column, so a wrongly-cased
/// stored value like `"High"` ranks as unknown (`-1`, effectively excluded)
/// there but `2` here, since this function lowercases before matching.
/// Unreachable today — triage validates the priority enum before it ever
/// writes a row, so a mismatched case cannot land in the database — but
/// worth knowing if that write-path validation is ever relaxed.
fn priority_rank(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "low" => Some(0),
        "normal" => Some(1),
        "high" => Some(2),
        "critical" => Some(3),
        _ => None,
    }
}
