//! The rule document: what a rule *is*, how it parses out of TOML, and how
//! its untrusted regexes are bounded before anything runs them.
//!
//! # Every regex in here is untrusted input
//!
//! A rule's patterns arrive from a user — typed into `mail rule add`, sent
//! over `RuleService.CreateRule`, or proposed by a model in
//! [`super::synth`]. They are then evaluated, unattended, against every new
//! message. That makes the pattern the single most dangerous field in this
//! module, and [`compile_pattern`] is the only place one is ever turned into
//! a matcher.
//!
//! Three bounds apply, and they cover different failure modes:
//!
//! - **Pattern length** ([`RuleLimits::max_pattern_len`]). A megabyte-long
//!   alternation is rejected before the regex crate ever sees it, so the
//!   cheapest attack costs nothing to refuse.
//! - **Compiled program size** ([`RuleLimits::regex_size_limit_bytes`], via
//!   [`regex::RegexBuilder::size_limit`]) and nesting depth. This is what
//!   stops the counted-repetition blow-up — `(a{1000}){1000}` is a short
//!   string that expands to an enormous automaton — from becoming a
//!   multi-second compile and hundreds of megabytes of resident memory.
//!   `build()` returns an error instead, which surfaces as a plain
//!   [`Error::InvalidArgument`] at the moment the rule is created rather than
//!   as a wedged evaluator hours later.
//! - **Haystack length** ([`RuleLimits::max_match_chars`]). Matching is
//!   linear in the input, so a large body is a slow match rather than an
//!   unbounded one — but "linear in a 40 MB message body, times every regex
//!   in every rule, on every new message" is still a denial of service worth
//!   refusing. Fields are truncated at a character boundary before matching.
//!
//! What is deliberately *not* here is a timeout thread or a watchdog. The
//! `regex` crate compiles to a finite automaton and has no backtracking, so
//! its match time is O(pattern × haystack) with no pathological case to time
//! out of — the classic "catastrophic backtracking" attack this kind of
//! bound usually exists to stop cannot be expressed. The bounds above are on
//! *compilation* and on *input size*, which are the two dimensions that
//! actually remain. `rules::tests::a_counted_repetition_bomb_is_refused_at_
//! compile_time_not_at_match_time` is the regression proof.
//!
//! # Why the document is `[[rules]]`, not a bare table
//!
//! One shape parses both a whole rules file and a single rule submitted over
//! gRPC, so `mail rule add -f rules.toml`, the daemon's own config, and
//! `CreateRule` all read the same text. [`parse_document`] is the only
//! parser; [`parse_single`] is it plus a "exactly one" check, which is what
//! `CreateRule` needs so that one RPC creates one named row.

use std::collections::{BTreeMap, BTreeSet};

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The longest rule name accepted — long enough for a sentence fragment,
/// short enough that a listing stays readable.
pub const MAX_NAME_LEN: usize = 64;

/// The longest `claude_is` predicate accepted. This text is sent to a model
/// on every uncached classification, so an unbounded one is unbounded spend.
pub const MAX_CLAUDE_IS_LEN: usize = 500;

/// The longest rule document accepted, in bytes. A rule is a handful of
/// patterns; anything past this is not a rule.
pub const MAX_DOCUMENT_LEN: usize = 16 * 1024;

/// Bounds applied to every pattern a rule carries. Sourced from
/// [`crate::config::RulesConfig`] so an operator can tighten them, with
/// defaults chosen to be generous for real rules and hostile to pathological
/// ones — see the module docs for what each one actually stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleLimits {
    /// Maximum length, in bytes, of one regex's source text.
    pub max_pattern_len: usize,
    /// Maximum size, in bytes, of one compiled regex program.
    pub regex_size_limit_bytes: usize,
    /// Maximum characters of any one field a regex is matched against.
    pub max_match_chars: usize,
}

impl Default for RuleLimits {
    fn default() -> Self {
        Self {
            max_pattern_len: 512,
            regex_size_limit_bytes: 256 * 1024,
            max_match_chars: 64 * 1024,
        }
    }
}

/// How a rule's predicates combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Every configured predicate must match. The default, and the mode that
    /// makes the cost ordering in [`super::eval`] meaningful: one failed
    /// deterministic predicate is enough to skip the `claude_is` call
    /// entirely.
    #[default]
    All,
    /// Any one configured predicate matching is enough.
    Any,
}

/// A rules document: one or more `[[rules]]` elements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDocument {
    /// The rules, in document order.
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
}

/// One rule, exactly as authored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// Unique (per account, case-insensitively) human-chosen name.
    pub name: String,
    /// Whether the evaluator fires this rule. A disabled rule is still
    /// listed and still backtestable — writing one and validating it before
    /// turning it on is the intended flow.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How `when`'s predicates combine.
    #[serde(default, rename = "match", skip_serializing_if = "is_default_match")]
    pub match_mode: MatchMode,
    /// The predicates.
    pub when: Predicates,
    /// The actions fired when they match.
    pub then: Actions,
}

const fn default_true() -> bool {
    true
}

fn is_default_match(mode: &MatchMode) -> bool {
    matches!(mode, MatchMode::All)
}

/// The predicate block (`[rules.when]`).
///
/// `from`/`subject`/`body`/`header` values are **regular expressions**, not
/// substrings — an anchored pattern is how a rule says "exactly this", and
/// an unanchored one already behaves as a substring search. Everything here
/// is optional; a rule with no predicate at all is rejected by
/// [`RuleSpec::validate`] rather than silently matching every message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Predicates {
    /// Regex over the sender rendered as `Display Name <addr@example.com>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Regex over the subject line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Regex over the extracted plain-text body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// IMAP flags/keywords that must all be present.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub has_flags: Vec<String>,
    /// IMAP flags/keywords that must all be absent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lacks_flags: Vec<String>,
    /// Minimum RFC822 size in bytes, inclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_bytes: Option<u64>,
    /// Maximum RFC822 size in bytes, inclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// The natural-language predicate, answered by Claude and cached by
    /// message-id + prompt-hash. Costs a model call on a cache miss, which
    /// is why [`super::eval`] evaluates it last and only when the
    /// deterministic predicates have not already decided the rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_is: Option<String>,
    /// Header name (matched case-insensitively) to regex over its value.
    ///
    /// Last field on purpose: TOML requires a table's scalar keys to precede
    /// its sub-tables, and this is the only sub-table `[rules.when]` has.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,
}

impl Predicates {
    /// Whether any deterministic (non-`claude_is`) predicate is configured.
    #[must_use]
    pub fn has_deterministic(&self) -> bool {
        self.from.is_some()
            || self.subject.is_some()
            || self.body.is_some()
            || !self.header.is_empty()
            || !self.has_flags.is_empty()
            || !self.lacks_flags.is_empty()
            || self.min_bytes.is_some()
            || self.max_bytes.is_some()
    }

    /// Whether anything at all is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.has_deterministic() && self.claude_is.is_none()
    }
}

/// The action block (`[rules.then]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Actions {
    /// Move the message to this mailbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_to: Option<String>,
    /// Move the message to the configured archive mailbox
    /// ([`crate::config::RulesConfig::archive_mailbox`]). Mutually exclusive
    /// with `move_to` — two destinations for one message is a typo, not an
    /// intent this engine should guess at.
    #[serde(skip_serializing_if = "is_false")]
    pub archive: bool,
    /// Tags to apply (prd.md's "label").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_labels: Vec<String>,
    /// IMAP flags/keywords to add. Added to the message's existing set,
    /// never a replacement of it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_flags: Vec<String>,
    /// Publish a `RULE_FIRED` event on the durable log.
    #[serde(skip_serializing_if = "is_false")]
    pub notify: bool,
    /// Run this configured hook (by `[[hooks.hooks]] name`) with the rule
    /// event JSON on its stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_hook: Option<String>,
    /// Create a reply draft with this body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_reply: Option<String>,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

impl Actions {
    /// The TOML key of every action configured here, in the order
    /// [`super::ActionRunner::apply`] would fire them.
    ///
    /// Exists for the prompt-injection shield's withhold path
    /// ([`super::RuleEngine`]): reporting "actions withheld" as a single
    /// opaque line would tell a user nothing about what did not happen to
    /// their mail, and re-deriving the list at that call site would be a
    /// second copy of this struct's shape that drifts the first time an
    /// action is added.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if !self.add_labels.is_empty() {
            names.push("add_labels");
        }
        if !self.add_flags.is_empty() {
            names.push("add_flags");
        }
        if self.notify {
            names.push("notify");
        }
        if self.run_hook.is_some() {
            names.push("run_hook");
        }
        if self.draft_reply.is_some() {
            names.push("draft_reply");
        }
        if self.archive {
            names.push("archive");
        }
        if self.move_to.is_some() {
            names.push("move_to");
        }
        names
    }

    /// Whether any action is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.move_to.is_none()
            && !self.archive
            && self.add_labels.is_empty()
            && self.add_flags.is_empty()
            && !self.notify
            && self.run_hook.is_none()
            && self.draft_reply.is_none()
    }
}

/// Parse a rules document.
///
/// # Errors
/// [`Error::InvalidArgument`] if `text` is over [`MAX_DOCUMENT_LEN`], is not
/// valid TOML, carries an unknown key, or contains a rule that fails
/// [`RuleSpec::validate`] under `limits`.
pub fn parse_document(text: &str, limits: &RuleLimits) -> Result<Vec<RuleSpec>, Error> {
    let rules = parse_document_unvalidated(text)?;
    for rule in &rules {
        rule.validate(limits)?;
    }
    Ok(rules)
}

/// Parse a rules document's *shape* — length, TOML, unknown keys, duplicate
/// names — without validating or compiling any rule in it.
///
/// Split out from [`parse_document`] so a caller that is about to
/// [`RuleSpec::compile`] does not pay for a throw-away compilation first:
/// `compile` validates as part of building, and compiling every pattern twice
/// per rule per evaluation is real CPU on a Tokio worker. Callers that only
/// want to know whether a document is acceptable (and will not compile it
/// themselves) must use [`parse_document`].
///
/// # Errors
/// [`Error::InvalidArgument`] if `text` is over [`MAX_DOCUMENT_LEN`], is not
/// valid TOML, carries an unknown key, holds no `[[rules]]` entry, or names
/// the same rule twice.
pub fn parse_document_unvalidated(text: &str) -> Result<Vec<RuleSpec>, Error> {
    if text.len() > MAX_DOCUMENT_LEN {
        return Err(Error::invalid_argument(format!(
            "rule document is {} bytes; the limit is {MAX_DOCUMENT_LEN}",
            text.len()
        )));
    }
    let document: RuleDocument = toml::from_str(text)
        .map_err(|e| Error::invalid_argument(format!("rule document is not valid TOML: {e}")))?;
    if document.rules.is_empty() {
        return Err(Error::invalid_argument(
            "rule document contains no [[rules]] entries",
        ));
    }
    let mut seen = BTreeSet::new();
    for rule in &document.rules {
        if !seen.insert(rule.name.to_lowercase()) {
            return Err(Error::invalid_argument(format!(
                "rule document names {:?} more than once",
                rule.name
            )));
        }
    }
    Ok(document.rules)
}

/// Parse a document that must contain exactly one rule — what `CreateRule`
/// accepts, so that one RPC creates one named row.
///
/// # Errors
/// As [`parse_document`], plus [`Error::InvalidArgument`] if the document
/// holds more than one `[[rules]]` element.
pub fn parse_single(text: &str, limits: &RuleLimits) -> Result<RuleSpec, Error> {
    let mut rules = parse_document(text, limits)?;
    if rules.len() > 1 {
        return Err(Error::invalid_argument(format!(
            "expected exactly one [[rules]] entry, found {}; create them one at a time",
            rules.len()
        )));
    }
    // `parse_document` already rejected the empty case, so `remove(0)` cannot
    // panic — but `pop` says so without relying on that reading.
    rules.pop().ok_or_else(|| {
        Error::internal("rule document passed validation with no rules in it".to_owned())
    })
}

/// Render one rule back to a `[[rules]]` document — what `SynthesizeRule`
/// returns and what `CreateRule` would accept verbatim.
///
/// # Errors
/// [`Error::Internal`] if the rule cannot be serialized, which for this
/// (string/number/bool-only) shape means an allocation failure rather than a
/// data problem.
pub fn to_document(rule: &RuleSpec) -> Result<String, Error> {
    let document = RuleDocument {
        rules: vec![rule.clone()],
    };
    toml::to_string_pretty(&document)
        .map_err(|e| Error::internal(format!("could not render rule as TOML: {e}")))
}

impl RuleSpec {
    /// Validate this rule, including compiling every pattern under `limits`.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] naming the specific field at fault.
    pub fn validate(&self, limits: &RuleLimits) -> Result<(), Error> {
        self.check(limits)?;
        // Compiling is the validation — a pattern that only fails at match
        // time is a rule that looked fine when it was created and silently
        // stops working later.
        Compiled::build(self, limits).map(|_| ())
    }

    /// Everything [`RuleSpec::validate`] checks *except* compiling the
    /// patterns, so [`RuleSpec::compile`] can do the compile half itself
    /// instead of doing it twice.
    fn check(&self, limits: &RuleLimits) -> Result<(), Error> {
        let _ = limits;
        validate_name(&self.name)?;
        if self.when.is_empty() {
            return Err(Error::invalid_argument(format!(
                "rule {:?} has no predicates; a rule that matches everything is never what \
                 was meant",
                self.name
            )));
        }
        if self.then.is_empty() {
            return Err(Error::invalid_argument(format!(
                "rule {:?} has no actions; add at least one of move_to/archive/add_labels/\
                 add_flags/notify/run_hook/draft_reply",
                self.name
            )));
        }
        if self.then.move_to.is_some() && self.then.archive {
            return Err(Error::invalid_argument(format!(
                "rule {:?} sets both move_to and archive; pick one destination",
                self.name
            )));
        }
        if let Some(claude_is) = &self.when.claude_is {
            let trimmed = claude_is.trim();
            if trimmed.is_empty() {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an empty claude_is predicate",
                    self.name
                )));
            }
            if trimmed.chars().count() > MAX_CLAUDE_IS_LEN {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has a claude_is predicate longer than {MAX_CLAUDE_IS_LEN} \
                     characters",
                    self.name
                )));
            }
        }
        if let (Some(min), Some(max)) = (self.when.min_bytes, self.when.max_bytes) {
            if min > max {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has min_bytes ({min}) above max_bytes ({max})",
                    self.name
                )));
            }
        }
        for (label, flags) in [
            ("has_flags", &self.when.has_flags),
            ("lacks_flags", &self.when.lacks_flags),
        ] {
            for flag in flags {
                if !crate::mail::is_safe_flag(flag.trim()) {
                    return Err(Error::invalid_argument(format!(
                        "rule {:?} has an unusable entry in {label}: {flag:?}",
                        self.name
                    )));
                }
            }
        }
        for name in &self.then.add_labels {
            if name.trim().is_empty() {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an empty entry in add_labels",
                    self.name
                )));
            }
        }
        for flag in &self.then.add_flags {
            // The same boundary check `MailStore::set_flags` applies, applied
            // here so an unusable flag is refused when the rule is written
            // rather than surfacing as a reported action failure every time it
            // fires. `crate::mail::is_safe_flag`'s own docs explain what it
            // guards: these strings are joined into an IMAP `FLAGS (...)`
            // argument, so a space or a parenthesis is command injection.
            if !crate::mail::is_safe_flag(flag.trim()) {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an unusable entry in add_flags: {flag:?}",
                    self.name
                )));
            }
        }
        if let Some(hook) = &self.then.run_hook {
            if hook.trim().is_empty() {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an empty run_hook name",
                    self.name
                )));
            }
        }
        if let Some(mailbox) = &self.then.move_to {
            if mailbox.trim().is_empty() {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an empty move_to mailbox",
                    self.name
                )));
            }
        }
        Ok(())
    }

    /// Compile this rule's patterns for evaluation.
    ///
    /// Equivalent to [`RuleSpec::validate`] followed by a build, except that
    /// the patterns are compiled exactly *once* rather than twice — which
    /// matters because this runs per rule per message batch on a Tokio worker.
    ///
    /// # Errors
    /// As [`RuleSpec::validate`].
    pub fn compile(&self, limits: &RuleLimits) -> Result<Compiled, Error> {
        self.check(limits)?;
        Compiled::build(self, limits)
    }
}

/// A rule name that is present, trimmed, bounded, and free of control
/// characters.
///
/// # Errors
/// [`Error::InvalidArgument`] describing which of those it failed.
pub fn validate_name(name: &str) -> Result<&str, Error> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(Error::invalid_argument("rule name must not be empty"));
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(Error::invalid_argument(format!(
            "rule name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(Error::invalid_argument(
            "rule name must not contain control characters",
        ));
    }
    Ok(trimmed)
}

/// A rule with its patterns compiled and its limits captured.
#[derive(Debug, Clone)]
pub struct Compiled {
    /// The rule as authored.
    pub spec: RuleSpec,
    /// Compiled `from` pattern.
    pub from: Option<Regex>,
    /// Compiled `subject` pattern.
    pub subject: Option<Regex>,
    /// Compiled `body` pattern.
    pub body: Option<Regex>,
    /// Compiled header patterns, keyed by the lowercased header name.
    pub header: Vec<(String, Regex)>,
    /// The bounds this was compiled under; [`super::eval`] reads
    /// `max_match_chars` from here so the truncation and the compilation can
    /// never be configured out of step.
    pub limits: RuleLimits,
}

impl Compiled {
    fn build(spec: &RuleSpec, limits: &RuleLimits) -> Result<Self, Error> {
        let mut header = Vec::with_capacity(spec.when.header.len());
        for (name, pattern) in &spec.when.header {
            if name.trim().is_empty() {
                return Err(Error::invalid_argument(format!(
                    "rule {:?} has an empty header name",
                    spec.name
                )));
            }
            header.push((
                name.trim().to_ascii_lowercase(),
                compile_pattern(&format!("header.{name}"), pattern, limits)?,
            ));
        }
        Ok(Self {
            spec: spec.clone(),
            from: spec
                .when
                .from
                .as_deref()
                .map(|p| compile_pattern("from", p, limits))
                .transpose()?,
            subject: spec
                .when
                .subject
                .as_deref()
                .map(|p| compile_pattern("subject", p, limits))
                .transpose()?,
            body: spec
                .when
                .body
                .as_deref()
                .map(|p| compile_pattern("body", p, limits))
                .transpose()?,
            header,
            limits: *limits,
        })
    }
}

/// Compile one untrusted pattern under `limits`. The one place in this crate
/// a rule's regex is built — see the module docs for what each bound stops.
///
/// # Errors
/// [`Error::InvalidArgument`] if the source is over
/// [`RuleLimits::max_pattern_len`], is not a valid regex, or compiles to a
/// program larger than [`RuleLimits::regex_size_limit_bytes`].
pub fn compile_pattern(field: &str, pattern: &str, limits: &RuleLimits) -> Result<Regex, Error> {
    if pattern.len() > limits.max_pattern_len {
        return Err(Error::invalid_argument(format!(
            "{field} pattern is {} bytes; the limit is {}",
            pattern.len(),
            limits.max_pattern_len
        )));
    }
    RegexBuilder::new(pattern)
        .size_limit(limits.regex_size_limit_bytes)
        // The match-time lazy-DFA cache, bounded separately from the program
        // itself: a pattern small enough to compile can still ask for an
        // unbounded cache while matching. The regex crate falls back to a
        // slower engine rather than failing when this is hit, so it costs
        // throughput, never correctness.
        .dfa_size_limit(limits.regex_size_limit_bytes)
        // Bounds parser recursion so a deeply nested group cannot overflow
        // the stack during parsing. Well above anything a real rule nests.
        .nest_limit(64)
        .build()
        .map_err(|e| Error::invalid_argument(format!("{field} pattern is not usable: {e}")))
}

/// Truncate `text` to at most `max_chars` characters, at a character
/// boundary. Applied to every field before a regex sees it — see the module
/// docs' haystack-length bound.
#[must_use]
pub fn bounded<'a>(text: &'a str, limits: &RuleLimits) -> &'a str {
    match text.char_indices().nth(limits.max_match_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}
