//! The operator grammar: deterministic parsing of a raw query string into
//! hard filters plus ranked free text (prd.md, "Query Language / Operators").
//!
//! # Filters gate, free text ranks
//!
//! `from:alice invoice` is not "two words that must both match" — `from:`
//! becomes a `WHERE` constraint applied before any retriever runs, and
//! `invoice` is a term the lexical and semantic retrievers rank on. Keeping
//! that split explicit in the return type ([`ParsedQuery`]) is the entire
//! point of this module: everything downstream (candidate generation,
//! fusion, the L1/L2 rankers) needs to know which half of the query it is
//! looking at.
//!
//! # Parsing never fails
//!
//! [`parse`] returns a [`ParsedQuery`], not a `Result`. A search box is not a
//! programming-language front end — a user who types `date:not-a-range`, a
//! lone `-`, an unterminated quote, or a `key:value` nobody registered did
//! not make a mistake worth an error dialog; they typed something this
//! parser does not have special handling for, and the only sane behavior is
//! to fall back to "search for this text". Concretely:
//!
//! - An unrecognized `key:` degrades the whole `key:value` token to a free-text
//!   term, verbatim.
//! - A recognized `key:` whose value doesn't fit that operator's shape (a
//!   `larger:` that isn't a size, a `date:` with no `..`) degrades the same
//!   way, for the same reason — the key doesn't get to demand a stricter
//!   value than free text can offer.
//! - An unterminated quote runs to the end of input rather than swallowing
//!   nothing or erroring.
//! - A bare `-`, `~`, or `=` with nothing following it is treated as the
//!   literal character, not as a modifier with no target.
//!
//! # Sigils apply to free text only
//!
//! The `~` (force semantic) and `=` (force lexical) prefixes are, per the
//! grammar, a way to override *ranking* for a term or phrase — "search this
//! one thing by meaning" or "match this one thing literally". Applying one to
//! an operator (`~tag:work`) is not meaningful: a hard filter is not ranked,
//! so there is nothing for a ranking-mode sigil to modify. Rather than guess
//! at intent, a sigil-prefixed token is never parsed as an operator — it
//! becomes a moded free-text term instead (`~tag:work` is a semantic search
//! for the literal text `tag:work`, not a tag filter).
//!
//! Negation (`-`) has no such conflict — `-tag:newsletter` excludes a filter
//! and `-excludeterm` excludes a term — so it composes with a sigil
//! (`-~urgent`). The order is fixed (negation is stripped first, then the
//! sigil): `~-x` and `=-x` are not read as negation of `x`, they are a moded
//! search for the literal text `-x`. Only the one order appears in the
//! grammar, so there is nothing to gain from accepting the other.

/// The parsed form of a raw query string.
///
/// This is the output of Stage 0's operator-parse step only (prd.md's
/// `QueryPlan` also carries intent, spelling corrections, expansions, and a
/// query embedding — those are assembled from a `ParsedQuery` in a later
/// task, not produced here).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ParsedQuery {
    /// The original, unmodified input. Kept for cache keys, `--explain`
    /// output, and re-parsing after a correction — losing it would make this
    /// type a one-way function of something a caller may need back.
    pub raw: String,
    /// Operators, each a `WHERE`-style hard constraint. Order matches the
    /// order operators appeared in `raw`, which is not meaningful for
    /// filtering (they conjoin) but is meaningful for showing a user back
    /// what was understood.
    pub filters: Vec<Filter>,
    /// Free-text words, ranked rather than filtered.
    pub terms: Vec<Term>,
    /// Free-text quoted phrases, ranked rather than filtered.
    ///
    /// Kept separate from [`terms`](Self::terms) rather than folded in
    /// because they are handled differently downstream: a phrase is a
    /// proximity/exact-adjacency signal for the lexical retriever, a term is
    /// a single-token match. Merging them here would make the retriever
    /// re-derive which is which from `text.contains(' ')` — fragile, and a
    /// property this type can just guarantee instead.
    pub phrases: Vec<Phrase>,
}

/// One recognized operator plus whether it was negated with a leading `-`.
///
/// Negation is factored out of [`Operator`] itself, rather than doubling
/// every variant (`From` / `NotFrom`, `Tag` / `NotTag`, ...), because it is
/// the same transformation — "exclude matches" — for every operator, and a
/// downstream `WHERE` builder can apply it uniformly (`AND NOT (...)`)
/// without a match arm per operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Filter {
    /// The operator and its value.
    pub op: Operator,
    /// `true` if the token carried a leading `-` (e.g. `-tag:newsletter`).
    pub negated: bool,
}

/// A recognized operator and its parsed value.
///
/// Every variant here corresponds to one entry in prd.md's operator grammar.
/// Sub-values that have their own small vocabulary ([`HasTarget`],
/// [`IsFlag`], [`AiPredicate`]) keep an `Other`/`Flag` escape hatch rather
/// than being a closed enum, for the same "never fail" reason [`parse`]
/// itself never returns `Result`: `has:calendar` from a future grammar
/// addition should keep working against this parser, filtered out downstream
/// if nothing recognizes it, rather than silently losing the operator and
/// falling back to a free-text search that means something different.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Operator {
    /// `from:` — sender address or name fragment.
    From(String),
    /// `to:` — a recipient (To).
    To(String),
    /// `cc:` — a recipient (Cc).
    Cc(String),
    /// `subject:` — subject line contains.
    Subject(String),
    /// `body:` — body contains (often paired with a quoted exact phrase).
    Body(String),
    /// `has:` — structural predicate (attachment, note, tag, ...).
    Has(HasTarget),
    /// `filename:` — attachment filename, glob pattern (`*.pdf`).
    Filename(String),
    /// `larger:` — message size strictly greater than this many bytes.
    Larger(u64),
    /// `smaller:` — message size strictly less than this many bytes.
    Smaller(u64),
    /// `before:` — date strictly before this value.
    ///
    /// The value is kept as written (`2025-01-01`, `last-week`, ...): parsing
    /// it into an absolute date requires the corpus-vocabulary-aware,
    /// possibly-NL date grammar the PRD assigns to Stage 0's later steps, not
    /// this one.
    Before(String),
    /// `after:` — date on or after this value. Same raw-value note as
    /// [`Before`](Self::Before).
    After(String),
    /// `on:` — date exactly matching this value. Same raw-value note as
    /// [`Before`](Self::Before).
    On(String),
    /// `date:start..end` — an inclusive date range. Both bounds are raw, as
    /// with [`Before`](Self::Before); only the `start..end` shape itself is
    /// validated here.
    DateRange(String, String),
    /// `is:` — a status flag (unread, flagged, ...).
    Is(IsFlag),
    /// `tag:` — a user tag, possibly hierarchical (`project/*`) or a glob.
    Tag(String),
    /// `note:` — a note's text contains.
    Note(String),
    /// `in:` — mailbox/folder scope (`INBOX`, `Archive`, ...).
    In(String),
    /// `account:` — configured account name.
    Account(String),
    /// `thread:` — a specific thread id.
    Thread(String),
    /// `ai:` — an AI-enrichment predicate (`needs-reply`, `priority>high`,
    /// `category:invoice`, ...).
    Ai(AiPredicate),
}

/// The value half of a `has:` operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HasTarget {
    /// `has:attachment`
    Attachment,
    /// `has:note`
    Note,
    /// `has:tag`
    Tag,
    /// Any value outside the documented set, preserved verbatim. See the
    /// module docs' note on why sub-value enums keep an escape hatch.
    Other(String),
}

impl HasTarget {
    fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "attachment" => Self::Attachment,
            "note" => Self::Note,
            "tag" => Self::Tag,
            _ => Self::Other(value.to_owned()),
        }
    }
}

/// The value half of an `is:` operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IsFlag {
    /// `is:unread`
    Unread,
    /// `is:read`
    Read,
    /// `is:flagged`
    Flagged,
    /// `is:pinned`
    Pinned,
    /// `is:replied`
    Replied,
    /// `is:muted`
    Muted,
    /// Any value outside the documented set, preserved verbatim.
    Other(String),
}

impl IsFlag {
    fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "unread" => Self::Unread,
            "read" => Self::Read,
            "flagged" => Self::Flagged,
            "pinned" => Self::Pinned,
            "replied" => Self::Replied,
            "muted" => Self::Muted,
            _ => Self::Other(value.to_owned()),
        }
    }
}

/// The value half of an `ai:` operator.
///
/// The grammar overloads `ai:` with three shapes — a bare flag
/// (`ai:needs-reply`), a key/value equality (`ai:category:invoice`), and a
/// key/threshold comparison (`ai:priority>high`) — and this mirrors that
/// rather than picking one. Which shape a given value took is decided purely
/// syntactically, by whichever of `>` or `:` occurs *first* in the value: an
/// earlier `>` means threshold, an earlier `:` means equality, and neither
/// means a flag. This stage does not know or care whether `priority` or
/// `category` are real enrichment fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AiPredicate {
    /// A bare flag with no comparator, e.g. `needs-reply`.
    Flag(String),
    /// `key:value`, e.g. `category` = `invoice`.
    Equals(String, String),
    /// `key>value`, e.g. `priority` > `high`.
    GreaterThan(String, String),
}

impl AiPredicate {
    /// Resolve an `ai:` value into its predicate shape, or `None` if a
    /// separator is present but leaves one side empty (`ai:>`, `ai:category:`,
    /// mid-keystroke input like `ai:priority>` typed one character at a
    /// time). That is not a flag with stray punctuation baked into its name —
    /// it is a malformed comparison, and per this module's degrade-never-error
    /// rule it falls back to free text like any other operator's bad value,
    /// rather than becoming a hard filter that can never match anything.
    /// A value with no `>` or `:` at all is always a valid flag.
    fn parse(value: &str) -> Option<Self> {
        // Whichever separator appears first decides the shape; see the type
        // docs. Comparing byte positions (not just "does `>` exist") is what
        // makes `ai:a:b>c` read as `Equals("a", "b>c")` rather than
        // `GreaterThan("a:b", "c")` — the `:` at index 1 precedes the `>` at
        // index 3.
        let threshold_at = value.find('>');
        let equals_at = value.find(':');
        let separator = match (threshold_at, equals_at) {
            (Some(t), Some(e)) if t < e => Some((t, true)),
            (Some(t), None) => Some((t, true)),
            (_, Some(e)) => Some((e, false)),
            (None, None) => None,
        };
        match separator {
            Some((at, is_threshold)) => {
                // Both separators are single ASCII bytes, so `at + 1` is
                // always a valid char boundary.
                let key = &value[..at];
                let val = &value[at + 1..];
                if key.is_empty() || val.is_empty() {
                    return None;
                }
                Some(if is_threshold {
                    Self::GreaterThan(key.to_owned(), val.to_owned())
                } else {
                    Self::Equals(key.to_owned(), val.to_owned())
                })
            }
            None => Some(Self::Flag(value.to_owned())),
        }
    }
}

/// How a term or phrase should be retrieved, per the `~`/`=` prefix sigils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// No sigil: ranked normally by the hybrid pipeline (lexical + semantic +
    /// fuzzy, fused).
    #[default]
    Auto,
    /// `~` prefix: force semantic/dense retrieval for this token, bypassing
    /// exact lexical matching.
    Semantic,
    /// `=` prefix: force literal lexical matching for this token, bypassing
    /// semantic recall and query expansion.
    Lexical,
}

/// A single ranked free-text word.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Term {
    /// The word, with any leading `-`/`~`/`=` already stripped.
    pub text: String,
    /// `true` if prefixed with `-` (exclude rather than rank).
    pub negated: bool,
    /// The retrieval mode requested by a `~`/`=` prefix, if any.
    pub mode: Mode,
    /// `true` if this term was shaped like `key:value` (a bare identifier
    /// followed by `:`) but degraded to free text anyway — an unregistered
    /// key, or a registered key whose value didn't fit its shape.
    ///
    /// A degraded operator and an ordinary word are otherwise
    /// indistinguishable once they land in `terms`, but they are not the
    /// same kind of mistake: `form:alice` (typo of `from:`) is a strong
    /// "did you mean" candidate that spell-fix/`--explain` in a later task
    /// can act on directly, while `invoice` is not. Recording it here means
    /// that later stage does not have to re-tokenize `raw` to recover
    /// information this parser already had.
    pub looked_like_operator: bool,
}

/// A quoted free-text phrase (`"exact phrase"`).
///
/// Structurally identical to [`Term`] but kept as a distinct type — see
/// [`ParsedQuery::phrases`] for why. It has no `looked_like_operator`
/// equivalent: an operator's key is never itself quoted (`"from":alice` is
/// not part of the grammar), so a phrase can never be a degraded operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Phrase {
    /// The phrase content, quotes and any leading `-`/`~`/`=` already
    /// stripped.
    pub text: String,
    /// `true` if prefixed with `-` (exclude rather than rank).
    pub negated: bool,
    /// The retrieval mode requested by a `~`/`=` prefix, if any.
    pub mode: Mode,
}

/// Parse a raw query string into hard filters and ranked free text.
///
/// See the module docs for the fallback rules this function follows — in
/// short, nothing about the input can make it fail; anything it doesn't
/// specifically recognize becomes a free-text term.
#[must_use]
pub fn parse(raw: &str) -> ParsedQuery {
    let mut filters = Vec::new();
    let mut terms = Vec::new();
    let mut phrases = Vec::new();

    for token in split_tokens(raw) {
        match classify(token) {
            Classified::Filter(filter) => filters.push(filter),
            // A `Term`'s text is never empty: `split_tokens` never emits an
            // empty token, and `classify` only strips a leading `-`/`~`/`=`
            // when something non-empty remains after it. The guard below is
            // for symmetry with `Phrase`, whose text *can* be empty (`""`
            // has nothing between its quotes) — applying the same check to
            // both costs nothing and keeps this loop from depending on that
            // invariant holding forever.
            Classified::Term(term) => {
                if !term.text.is_empty() {
                    terms.push(term);
                }
            }
            // An empty quoted phrase (`""`, or a lone `"` with nothing
            // after it) has no text to rank on, so it is dropped rather than
            // stored as a phrase nothing can ever match.
            Classified::Phrase(phrase) => {
                if !phrase.text.is_empty() {
                    phrases.push(phrase);
                }
            }
        }
    }

    ParsedQuery {
        raw: raw.to_owned(),
        filters,
        terms,
        phrases,
    }
}

/// One whitespace-delimited token, classified into what it turned out to be.
enum Classified {
    Filter(Filter),
    Term(Term),
    Phrase(Phrase),
}

/// Classify a single token (already split from the query on whitespace,
/// respecting quotes — see [`split_tokens`]) into a filter, term, or phrase.
fn classify(token: &str) -> Classified {
    let mut negated = false;
    let mut rest = token;
    if let Some(stripped) = rest.strip_prefix('-') {
        // A lone "-" negates nothing and is kept as the literal token rather
        // than silently vanishing — see the module docs' "bare modifier"
        // rule.
        if !stripped.is_empty() {
            negated = true;
            rest = stripped;
        }
    }

    let mut mode = Mode::Auto;
    if let Some(stripped) = rest.strip_prefix('~') {
        if !stripped.is_empty() {
            mode = Mode::Semantic;
            rest = stripped;
        }
    } else if let Some(stripped) = rest.strip_prefix('=') {
        if !stripped.is_empty() {
            mode = Mode::Lexical;
            rest = stripped;
        }
    }

    // A sigil means "rank this text a particular way", which only makes
    // sense for free text — see the module docs' "Sigils apply to free text
    // only" section. So operator parsing is only attempted when no sigil
    // fired.
    let mut looked_like_operator = false;
    if mode == Mode::Auto {
        if let Some((key, raw_value)) = split_operator(rest) {
            // Recognized-or-not, an operator-shaped token that doesn't
            // resolve to a filter below falls through to be treated as free
            // text, using `rest` (the original, still-quoted text) rather
            // than `value` — the degraded form is "search literally for what
            // was typed", not "search for the value with its key stripped".
            // `looked_like_operator` records the shape for whoever consumes
            // the resulting `Term` (see its doc comment) even though this
            // function itself has moved on to free text.
            looked_like_operator = true;
            let value = unquote(raw_value);
            if !value.is_empty() {
                if let Some(op) = parse_operator(key, &value) {
                    return Classified::Filter(Filter { op, negated });
                }
            }
        }
    }

    if let Some(phrase_text) = rest.strip_prefix('"') {
        Classified::Phrase(Phrase {
            text: unquote_body(phrase_text),
            negated,
            mode,
        })
    } else {
        Classified::Term(Term {
            text: rest.to_owned(),
            negated,
            mode,
            looked_like_operator,
        })
    }
}

/// Split `raw` into whitespace-separated tokens, treating a double-quoted
/// span as atomic even when it contains whitespace: `body:"exact phrase"`
/// stays one token, and so does a bare `"multi word phrase"`.
///
/// An unterminated quote (no closing `"` before the input ends) is not
/// special-cased — the loop simply never sees the whitespace inside it as a
/// split point, so the unterminated span runs to the end of the string. That
/// is the intended degradation: "the rest of the query is one phrase" rather
/// than a parse error.
fn split_tokens(raw: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut in_quotes = false;
    let mut start: Option<usize> = None;

    for (i, c) in raw.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c.is_whitespace() && !in_quotes {
            if let Some(s) = start.take() {
                tokens.push(&raw[s..i]);
            }
            continue;
        }
        start.get_or_insert(i);
    }
    if let Some(s) = start {
        tokens.push(&raw[s..]);
    }
    tokens
}

/// Split a token into `(key, value)` on its first `:`, if it is shaped like
/// an operator at all.
///
/// Only the *first* colon matters — `subject:"re: invoice"` must split into
/// `subject` / `"re: invoice"`, not stop at the colon inside the quoted
/// value. A key must look like a bare identifier (letters, digits, `_`,
/// `-`): this is what rejects a quoted phrase that happens to contain a
/// colon (`"re:invoice"` — the key would be `"re`, disqualified by its
/// leading `"`) and an address-shaped term (`user@host:port` — disqualified
/// by the `@`) before either could be mistaken for an operator. It does
/// *not* reject every non-operator — an identifier-shaped key that simply
/// isn't registered (`10:30`, or the host portion of a bare
/// `https://example.com`) still reaches [`parse_operator`], which is what
/// actually declines those by finding no matching key.
fn split_operator(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((key, value))
}

/// Resolve `(key, value)` — `value` already unquoted — into an [`Operator`],
/// or `None` if `key` is not a registered operator name or `value` doesn't
/// fit that operator's shape.
///
/// `None` here is not an error: every call site treats it as "fall back to
/// free text", per the module docs.
fn parse_operator(key: &str, value: &str) -> Option<Operator> {
    match key.to_ascii_lowercase().as_str() {
        "from" => Some(Operator::From(value.to_owned())),
        "to" => Some(Operator::To(value.to_owned())),
        "cc" => Some(Operator::Cc(value.to_owned())),
        "subject" => Some(Operator::Subject(value.to_owned())),
        "body" => Some(Operator::Body(value.to_owned())),
        "has" => Some(Operator::Has(HasTarget::parse(value))),
        "filename" => Some(Operator::Filename(value.to_owned())),
        "larger" => parse_size_bytes(value).map(Operator::Larger),
        "smaller" => parse_size_bytes(value).map(Operator::Smaller),
        "before" => Some(Operator::Before(value.to_owned())),
        "after" => Some(Operator::After(value.to_owned())),
        "on" => Some(Operator::On(value.to_owned())),
        "date" => parse_date_range(value),
        "is" => Some(Operator::Is(IsFlag::parse(value))),
        "tag" => Some(Operator::Tag(value.to_owned())),
        "note" => Some(Operator::Note(value.to_owned())),
        "in" => Some(Operator::In(value.to_owned())),
        "account" => Some(Operator::Account(value.to_owned())),
        "thread" => Some(Operator::Thread(value.to_owned())),
        "ai" => AiPredicate::parse(value).map(Operator::Ai),
        _ => None,
    }
}

/// Parse `date:start..end` into its two bounds.
///
/// The grammar documents only the closed-range form (both bounds present);
/// an open-ended range (`date:..2025-08`) is not part of it, so rather than
/// guess at a meaning for the missing side, that shape degrades to free text
/// like any other malformed operator value. Similarly, a *second* `..`
/// (`date:2025-06..2025-07..2025-08`) is rejected rather than silently
/// folded into the end bound (`split_once` would otherwise hand back
/// `"2025-07..2025-08"` as `end`) — it is not the documented shape either,
/// and guessing which range the user meant is no better than guessing which
/// side of a missing bound they meant.
fn parse_date_range(value: &str) -> Option<Operator> {
    let (start, end) = value.split_once("..")?;
    if start.is_empty() || end.is_empty() || end.contains("..") {
        return None;
    }
    Some(Operator::DateRange(start.to_owned(), end.to_owned()))
}

/// Parse a human size (`5mb`, `500kb`, `2gb`, `900b`, or a bare byte count)
/// into bytes.
///
/// Decimal (SI) units, not binary — `mb` means 1,000,000 bytes. This matches
/// the size a mail provider shows next to an attachment, so `larger:5mb`
/// agrees with "5 MB" in the message list instead of quietly meaning
/// something 5% larger.
fn parse_size_bytes(value: &str) -> Option<u64> {
    let lower = value.trim().to_ascii_lowercase();
    let (digits, multiplier): (&str, f64) = if let Some(n) = lower.strip_suffix("gb") {
        (n, 1_000_000_000.0)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1_000_000.0)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, 1_000.0)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1.0)
    } else {
        (lower.as_str(), 1.0)
    };
    let digits = digits.trim();
    if digits.is_empty() {
        return None;
    }
    let magnitude: f64 = digits.parse().ok()?;
    if !magnitude.is_finite() || magnitude < 0.0 {
        return None;
    }
    let bytes = magnitude * multiplier;
    // `as u64` on an out-of-range float saturates rather than panicking or
    // wrapping (defined Rust behavior since 1.45) — which is exactly the
    // failure mode to avoid here. A saturated `larger:9e99gb` would still
    // read as "bigger than anything", but the same saturation on
    // `smaller:9e99gb` reads as `Smaller(u64::MAX)` — no constraint at all,
    // the opposite of what was typed. Rejecting anything that doesn't fit in
    // a `u64` keeps both operators honest by degrading to free text instead,
    // same as any other value that doesn't fit its operator's shape.
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes.round() as u64)
}

/// Strip a leading `"` and, only if one was present, a matching trailing `"`.
///
/// Used for an operator's value (`raw_value` in `classify`), which may or may
/// not have been quoted in the source (`from:alice` vs. `body:"exact
/// phrase"`) — this normalizes both to the bare value. The trailing strip is
/// conditioned on the leading one having matched, not attempted
/// unconditionally: `from:alice"` has no opening quote, so the trailing `"`
/// is a character the user typed, not a delimiter, and must survive into
/// `Operator::From("alice\"")` rather than being silently eaten.
fn unquote(value: &str) -> String {
    match value.strip_prefix('"') {
        Some(body) => body.strip_suffix('"').unwrap_or(body).to_owned(),
        None => value.to_owned(),
    }
}

/// Strip a single trailing `"` from the remainder of an already-opened
/// phrase (the leading `"` was already consumed by the caller to detect that
/// this is a phrase at all).
///
/// A missing trailing quote is the unterminated-phrase case documented on
/// [`split_tokens`]; it is not an error here either, just a phrase whose text
/// runs to the end of the token.
fn unquote_body(body: &str) -> String {
    body.strip_suffix('"').unwrap_or(body).to_owned()
}

#[cfg(test)]
mod tests;
