//! The `:` command grammar (prd.md's Neovim-style commands; task 88): a verb
//! registry, and a pure parser/completer over it.
//!
//! # One shared vocabulary, not a second one
//!
//! [`crate::keymap::Action`] is already a stable, dotted-id namespace —
//! `keys.toml` binds it, `?` renders it, the palette resolves it. This
//! module does not invent a parallel command namespace: dots and spaces are
//! the same separator, so `message.archive` and `message archive` name the
//! same verb, and **every existing [`Action::id`] is already a valid verb
//! with no registry entry written for it** — see [`registry`]. A verb
//! declared *without* an [`Action`] behind it — a future task's `:tag`,
//! `:rule`, `:ai budget` — that also carries a [`Capability`] (a
//! [`crate::parity::Command`]) is checked in `tests` against that
//! capability's own CLI spelling, so the two surfaces cannot drift apart by
//! accident; an action-backed verb is deliberately exempt (its path is the
//! action id, which predates this grammar and must stay typeable
//! regardless of what a capability's `cli()` says) — see that module's
//! `spells_like_its_capability` for exactly what is and is not checked, and
//! why.
//!
//! # Shape
//!
//! - [`Verb`] — one command: a path (`&["message", "archive"]`), the
//!   optional capability and action it reaches, and its positionals/flags.
//! - [`registry`] — every verb, auto-derived from [`Action::ALL`] plus
//!   whatever `explicit` declares. Lazily built once; nothing here differs
//!   run to run.
//! - [`parse`] — text to a [`Resolution`], or a [`CommandError`] naming the
//!   offending token, in [`crate::keymap::KeymapError`]'s own idiom.
//! - [`complete`] — the candidates for whatever is typed so far, positional
//!   by cursor position (verb path, then that verb's own flags). Task 91's
//!   WhichKey band renders this the same way it renders a pending chord's
//!   continuations — one "what can I type next" surface, two data sources.
//!
//! # What this task does not do
//!
//! Parse only. Nothing here dispatches a [`Resolution`] to a `Cmd`, opens an
//! overlay, or touches `rmail-cli` at all — that is task 89's
//! `Overlay::Command`/`run_command`. This module has to exist and be fully
//! tested first, the same way [`crate::keymap`]'s engine predates task 85's
//! overlays that key off it.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use crate::keymap::Action;
use crate::parity::Command as Capability;

/// The largest count a range may hold, mirroring
/// [`crate::keymap::MAX_COUNT`] — a held-down digit key is a stuck key, not
/// a request to select nine thousand messages.
pub const MAX_COUNT: u32 = crate::keymap::MAX_COUNT;

// ---------------------------------------------------------------------------
// verbs
// ---------------------------------------------------------------------------

/// A positional argument a verb accepts, in declared order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Positional {
    /// What `describe`/an error names it — `"folder"`, `"query"`.
    pub name: &'static str,
    /// Whether the verb refuses without it.
    pub required: bool,
}

/// A `--flag` a verb accepts. Long-only, per the grammar's own rule — "no
/// `-a`": one spelling per concept, and a short form is a `clap` affordance
/// this grammar does not need to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The name, without its leading `--`.
    pub name: &'static str,
    /// Whether `--name value` (true) or a bare `--name` switch (false).
    pub takes_value: bool,
}

/// One command: a path in the verb registry, and what it reaches.
///
/// A leaf and an interior node are not different types — a [`Verb`] with no
/// [`Verb::action`] and no [`Verb::capability`] and no children is simply
/// unreachable, and nothing constructs one of those. What makes a path an
/// *interior* node (`:tag` alone opening a WhichKey band of its children,
/// per task 91, rather than erroring) is that no [`Verb`] in the registry
/// has exactly that path — [`parse`] tells the two cases apart by asking
/// the registry, not by a flag on this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verb {
    /// The full path, one segment per word — `&["message", "archive"]`,
    /// never the joined `"message archive"` (joining is [`Verb::canonical`],
    /// one direction only, so there is one place that decides the
    /// separator).
    pub path: Vec<&'static str>,
    /// The capability this verb reaches, if it reaches one directly. A verb
    /// with a bare [`Verb::action`] and no capability is normal — most of
    /// [`crate::keymap::Action`]'s local, UI-only actions (`cursor.down`,
    /// `help`) have no RPC behind them at all.
    pub capability: Option<Capability>,
    /// The action this verb delegates to when called with no arguments —
    /// see the module docs on task 89's dispatch rule. `None` for a verb
    /// this grammar is the *only* way to reach (no chord binds it).
    pub action: Option<Action>,
    /// Positional arguments, in the order they are read.
    pub positionals: &'static [Positional],
    /// Flags this verb accepts, valid in any position after the path.
    pub flags: &'static [Flag],
    /// A CLI spelling this verb is declared to reproduce even though it
    /// differs from [`crate::parity::Command::cli`] — the escape hatch
    /// `tests` needs for `tag-rules` (task 95's `:tag rules set`, nested,
    /// against the CLI's clap-flattened `tag-rules set`): a deliberate,
    /// declared choice to diverge, not a drift the check should catch.
    /// `None` for every verb that just spells things the way its
    /// capability already does.
    pub cli_alias: Option<&'static str>,
    /// A description for a verb reaching neither an action nor a capability
    /// — `describe`'s last resort before falling back to the bare path.
    /// `None` for every verb an action or a capability already describes;
    /// `Some` only exists so a verb like `:set`, local to the grammar with
    /// nothing behind it to borrow a sentence from, does not read as its own
    /// path twice in the generated command index.
    pub description: Option<&'static str>,
}

impl Verb {
    /// The path, space-joined — what `describe`, an error message, or
    /// completion shows a human. The one direction [`Verb::path`] is ever
    /// joined; parsing goes the other way, splitting on both `.` and ` `.
    #[must_use]
    pub fn canonical(&self) -> String {
        self.path.join(" ")
    }

    /// One line describing this verb — the action's own description if it
    /// has one, the capability's summary otherwise, this verb's own
    /// [`Verb::description`] if it was given one, and a bare statement of
    /// the path only if none of those apply (an interior node has no
    /// [`Verb`] at all, so every real [`Verb`] reaches at least one of the
    /// first three). Action first, the same precedence `tests`'
    /// `spells_like_its_capability` gives path spelling: for an auto-derived
    /// verb the action *is* the specific thing (`message.reply` and
    /// `message.forward` share one capability summary — "create a draft,
    /// optionally pre-filled..." — but each has its own action
    /// description), and a capability-only verb has no action to prefer
    /// over it anyway.
    #[must_use]
    pub fn describe(&self) -> String {
        if let Some(action) = self.action {
            return action.describe().to_owned();
        }
        if let Some(capability) = self.capability {
            return capability.summary().to_owned();
        }
        if let Some(description) = self.description {
            return description.to_owned();
        }
        self.canonical()
    }
}

/// Split `keys.toml`/an [`Action::id`]'s spelling into path segments: dots
/// and spaces are the same separator (the module docs' "one transform"),
/// and either is accepted on input. Empty segments (`"a..b"`, a leading or
/// trailing separator) are dropped rather than producing an empty path
/// element no [`Verb::path`] could ever contain.
fn split_path(text: &str) -> Vec<&str> {
    text.split(['.', ' '])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// the registry
// ---------------------------------------------------------------------------

/// Verbs declared beyond what [`Action::ALL`] gives for free.
///
/// Task 88's own job was the parser and the auto-derivation — domains get
/// their real verbs (`:tag`, `:rule`, `:ai budget`, …) from the tasks that
/// actually build what those verbs would call (94 onward), the same way
/// [`crate::keymap::Action`] grew one variant at a time rather than every
/// future task's binding being declared up front. A declaration here with
/// no task behind it yet would be exactly the half-finished state this
/// project's non-negotiables refuse.
///
/// The first two entries are task 103's, and they are the same action
/// twice. [`Action::ManualGrep`] needs a *declared* positional (`:helpgrep
/// invoice`), which an auto-derived verb has none of — the spelling
/// difference between a grammar that can describe itself and one that
/// quietly accepts an argument it never mentions. And it needs two paths:
/// `manual grep` because that is what its id spells, so `keys.toml` reads
/// `g/ = "manual.grep"` next to `<c-o> = "manual.back"` rather than one odd
/// sibling; and `helpgrep`, because that is what vim calls it and what task
/// 103's acceptance names. Declaring either suppresses the auto-derivation
/// (see [`registry`]), so both have to be written here or `manual grep`
/// would stop resolving.
///
/// Task 89 tried to add a third — `manual` with an optional `page`, so
/// `:manual archive` could carry one — and
/// `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` refused
/// it: a verb taking positionals must not be a strict prefix of another, or
/// the one word that collides (`grep`, here) silently means the longer verb
/// instead of being that argument. Declaring it anyway to serve a
/// convenience would have made the guard advisory. The page-name seam is
/// therefore still `rmail_cli::tui::model::open_manual_at`, called directly,
/// which is what task 102's `K`-on-a-key-reference-row does.
///
/// The third entry is task 93's `:set` — no action, no capability, and so
/// the first real verb to need [`Verb::description`]: neither of the other
/// two sources `Verb::describe` prefers has anything to say about it.
///
/// The last two are task 90's, and they are the first verbs here that reach a
/// [`Capability`] with **no** [`Action`] behind them — the shape
/// `tests::every_declared_verb_spells_its_capability_like_the_cli` was written
/// for and had until now no registry entry to check. `ClientAuthService` is
/// the one capability family no later task in `tasks.md` claims, and a TUI
/// that cannot answer "does this daemon want a password" has to be quit and
/// re-entered through `mail auth status` to find out; both paths spell the
/// verb exactly as `mail` does, so `spells_like_its_capability` holds with no
/// [`Verb::cli_alias`]. They also make task 90's Report reachable by typing:
/// `:auth status` renders one, and `auth clear` is the mutating row that
/// report's confirmation gate is about.
///
/// A function, not a `const` slice: [`Verb::path`] is a `Vec`, which cannot
/// appear in a `const` initializer at all (`Vec::new` allocates), so a
/// `const EXPLICIT: &[Verb] = &[]` can never actually gain an entry no
/// matter what a later task writes here — it would need this type changed
/// out from under it first. A plain function has no such ceiling.
fn explicit() -> Vec<Verb> {
    /// Optional, not required. A bare `:helpgrep` opens the same prompt the
    /// `g/` binding does, which is more useful than
    /// [`CommandError::MissingPositional`] — and, before task 89 puts a
    /// command line on screen, it is the only way the verb is reachable at
    /// all. `rmail_cli::tui::model::open_manual_grep_for` is what consumes
    /// the argument when there is one.
    const PATTERN: &[Positional] = &[Positional {
        name: "pattern",
        required: false,
    }];
    vec![
        Verb {
            path: vec!["manual", "grep"],
            capability: None,
            action: Some(Action::ManualGrep),
            positionals: PATTERN,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["helpgrep"],
            capability: None,
            action: Some(Action::ManualGrep),
            positionals: PATTERN,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["set"],
            capability: None,
            // No delegate: nothing binds `set` to a chord, and the two
            // positionals below (an option name and its value) are not
            // something an `Action` can carry, the same reason `manual grep`
            // has none either. Task 93's only tunables are the pane widths
            // and the AI panel width; task 101's `Screen::Settings` is the
            // fuller surface, not a second grammar for the same option
            // names.
            action: None,
            // Both optional, like `manual grep`'s `PATTERN` — required: true
            // would make bare `set` fail to parse at all, which
            // `every_real_verb_is_reachable_by_typing_its_own_path` refuses
            // for *every* real verb, no exceptions. `rmail_cli`'s
            // `set_option` is where "an option and a value are both
            // mandatory to do anything" is actually enforced — a semantic
            // question the grammar has no business answering.
            positionals: &[
                Positional {
                    name: "option",
                    required: false,
                },
                Positional {
                    name: "value",
                    required: false,
                },
            ],
            flags: &[],
            cli_alias: None,
            description: Some(
                "resize a pane or the AI panel — both an option and a value are required",
            ),
        },
        Verb {
            path: vec!["auth", "status"],
            capability: Some(Capability::ClientAuthAuthStatus),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["auth", "clear"],
            capability: Some(Capability::ClientAuthClearPassword),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
    ]
}

/// Every verb: [`explicit`] plus one auto-derived from each [`Action`] that
/// [`explicit`] does not already cover.
///
/// Built once — nothing here is request-dependent — behind a [`OnceLock`]
/// rather than a `const`, because splitting an [`Action::id`] into path
/// segments is a runtime `str::split`, not something `const fn` can do in
/// today's Rust. The pieces themselves are still `&'static str`: slicing a
/// `&'static str` produces `&'static str` slices, so no allocation survives
/// past the `Vec<&'static str>` each [`Verb::path`] holds.
fn registry() -> &'static [Verb] {
    static REGISTRY: OnceLock<Vec<Verb>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut verbs: Vec<Verb> = explicit();
        for action in Action::ALL {
            if verbs.iter().any(|verb| verb.action == Some(*action)) {
                continue;
            }
            verbs.push(Verb {
                path: split_path(action.id()),
                capability: Capability::for_action(*action).next(),
                action: Some(*action),
                positionals: &[],
                flags: &[],
                cli_alias: None,
                description: None,
            });
        }
        verbs
    })
}

/// The verb at exactly this path, if the registry has one.
///
/// Distinguishes a real verb from an interior node: `:tag` with no verb at
/// that exact path is not [`CommandError::UnknownVerb`], it is the
/// [`Resolution::Children`] case [`parse`] returns instead — see that
/// variant's docs.
#[must_use]
pub fn verb_at(path: &[&str]) -> Option<&'static Verb> {
    registry().iter().find(|verb| verb.path == path)
}

/// Every verb whose path is strictly longer than `prefix` and starts with
/// it.
///
/// The completion primitive: task 91's WhichKey band, told to render
/// verb-path completions, is this list grouped by each member's next
/// segment — the same "longest common prefix of the member ids" derivation
/// task 91's own `Keymap::continuations` (not yet written) uses for chords,
/// applied to verb paths instead of chord bindings.
#[must_use]
pub fn children_of(prefix: &[&str]) -> Vec<&'static Verb> {
    registry()
        .iter()
        .filter(|verb| verb.path.len() > prefix.len() && verb.path[..prefix.len()] == *prefix)
        .collect()
}

// ---------------------------------------------------------------------------
// ranges
// ---------------------------------------------------------------------------

/// The message set a command applies to — vim's range grammar, the one
/// place it has a genuine mail analogue (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// `'<,'>` — the active visual selection.
    Selection,
    /// `%` — every row in the current listing.
    All,
    /// A bare leading count — `N` messages from the cursor down. Saturates
    /// at [`MAX_COUNT`], the same policy [`crate::keymap::Pending`] applies
    /// to a chord's count for the same reason: a held-down digit key is not
    /// a request to allocate.
    Count(u32),
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection => f.write_str("'<,'>"),
            Self::All => f.write_str("%"),
            Self::Count(n) => write!(f, "{n}"),
        }
    }
}

// ---------------------------------------------------------------------------
// invocation
// ---------------------------------------------------------------------------

/// One flag as parsed: its name and, for a value-taking flag, the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFlag {
    /// The flag's name, without `--`.
    pub name: String,
    /// The value, for a flag [`Flag::takes_value`] declares one for.
    pub value: Option<String>,
}

/// A parsed, ready-to-dispatch `:` line — or, when no exact verb sits at
/// the resolved path, the interior-node case: a prompt naming what could
/// come next rather than an error (module docs' resolution algorithm, step
/// 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// An exact verb, with its arguments.
    Invocation(Box<Invocation>),
    /// `path` matched no exact verb, but is a strict prefix of at least
    /// one — `children` names every one of those; callers needing an order
    /// sort themselves. Never empty: an empty `children` would mean `path`
    /// matched nothing at all, which is [`CommandError::UnknownVerb`]
    /// instead.
    Children {
        /// The path typed so far.
        path: Vec<String>,
        /// Every verb this path is a strict prefix of.
        children: Vec<&'static Verb>,
    },
}

/// A parsed, ready-to-dispatch `:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The range prefix, if one was typed.
    pub range: Option<Range>,
    /// The verb's path — kept as owned segments rather than borrowing
    /// [`Verb::path`], so an [`Invocation`] does not tie its caller to the
    /// registry's lifetime for what is, after parsing, just data.
    pub verb: Vec<String>,
    /// The capability this invocation's verb reaches, if any.
    pub capability: Option<Capability>,
    /// The action this invocation's verb reaches, if any.
    pub action: Option<Action>,
    /// Positional arguments, in the order they were typed.
    pub positionals: Vec<String>,
    /// Flags, in the order they were typed.
    pub flags: Vec<ParsedFlag>,
    /// Whether a trailing `!` was present — task 89's "skip the
    /// confirmation overlay," and the *only* thing `!` means (module docs:
    /// "It never changes what a command does").
    pub bang: bool,
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Why a `:` line could not be parsed.
///
/// Every variant names the offending text, in [`crate::keymap::KeymapError`]'s
/// own idiom — these are read by someone who just typed the line, and
/// "invalid command" would leave them guessing which word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// Nothing but a range, or nothing at all.
    #[error("a command needs a verb")]
    Empty,
    /// A `'` that does not start the one range mark this grammar knows
    /// (`'<,'>`) — [`Range::Selection`]/[`Range::All`]/a count parse
    /// unconditionally, so this is specifically the range *token* itself
    /// being malformed, e.g. `'<,` with no closing `'>`, rather than a
    /// range with nothing to apply to (that is a caller's problem — task
    /// 89's, against a model that may have no visual selection — not a
    /// parse error).
    #[error("{text:?} looks like a range but is not '<,'>")]
    MalformedRange {
        /// The offending text, as written.
        text: String,
    },
    /// A `"` with no matching close.
    #[error("{text:?} has an unterminated quote")]
    UnterminatedQuote {
        /// The word being built when the quote failed to close — what was
        /// read after the opening `"` (plus anything glued before it), not
        /// the whole line, so the message points at the actual offending
        /// text.
        text: String,
    },
    /// No verb in the registry matches, and none has this as a strict
    /// prefix either — what [`parse`] returns when even
    /// [`Resolution::Children`] cannot apply.
    #[error("unknown command {path:?}{}", suggestion.as_deref().map_or(String::new(), |s| format!(" — did you mean `{s}`?")))]
    UnknownVerb {
        /// The path as typed, space-joined.
        path: String,
        /// The closest known verb's canonical path, if any looked close
        /// enough to name.
        suggestion: Option<String>,
    },
    /// `--name` where `name` is not one of the resolved verb's declared
    /// [`Flag`]s.
    #[error("{flag:?} is not a flag {verb} takes{}", if valid.is_empty() {
        " — it takes no flags".to_owned()
    } else {
        format!(" — try {}", valid.join(", "))
    })]
    UnknownFlag {
        /// The verb's canonical path.
        verb: String,
        /// The flag as typed, without `--`.
        flag: String,
        /// Every flag the verb does accept, `--`-prefixed.
        valid: Vec<String>,
    },
    /// A value-taking flag with nothing after it.
    #[error("--{flag} needs a value")]
    MissingFlagValue {
        /// The flag's name.
        flag: String,
    },
    /// Fewer positionals than the verb requires.
    #[error("{verb} needs {name} — try `{verb} <{name}>`")]
    MissingPositional {
        /// The verb's canonical path.
        verb: String,
        /// The missing positional's name.
        name: &'static str,
    },
}

// ---------------------------------------------------------------------------
// tokenizing
// ---------------------------------------------------------------------------

/// One raw token off the line, before verb/flag interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// A flag, `--name` or `--name=value` (the `=value` half already split
    /// off, so nothing downstream has to re-scan a token to tell the two
    /// spellings apart).
    Flag {
        name: String,
        value: Option<String>,
    },
}

/// Split a line into [`Token`]s: whitespace-separated except inside a
/// `"..."` quote, where `\"` escapes a literal quote and nothing else is
/// special — no `\n`, no `\\`, so a Windows path or a regex typed inside
/// quotes survives untouched. A quoted piece may be glued to more text
/// (`--query"a b"c` reads as one token `a bc`, the same way a shell would).
///
/// Deliberately not `.`-aware, even though a verb path is (module docs'
/// "one transform") — that has to be resolved by [`parse_verb`]/[`complete`]
/// instead, against the registry, because only they know where a verb path
/// ends and a positional or a flag value begins. Splitting bare words on
/// `.` here too would look tempting (a glued `message.archive` could
/// tokenize straight into two words) but is wrong: it would also fragment
/// `--since=2024.01.01` (a flag value) and any positional containing a
/// literal `.` (`report.pdf`, `3.14`, an email address) into several
/// tokens, silently. `tokenize` only ever sees undifferentiated words; it
/// cannot tell a verb segment from an argument, so it must not guess.
///
/// Never guesses at an unterminated quote as "assume it closes at the end
/// of the line" — that would silently produce different token boundaries
/// than the input actually has — it is [`CommandError::UnterminatedQuote`]
/// instead.
fn tokenize(text: &str) -> Result<Vec<Token>, CommandError> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' if chars.peek() == Some(&'"') => {
                            word.push('"');
                            chars.next();
                        }
                        other => word.push(other),
                    }
                }
                if !closed {
                    return Err(CommandError::UnterminatedQuote { text: word });
                }
                continue;
            }
            word.push(c);
            chars.next();
        }
        if let Some(name) = word.strip_prefix("--") {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (name.to_owned(), None),
            };
            tokens.push(Token::Flag { name, value });
        } else {
            tokens.push(Token::Word(word));
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// range prefix
// ---------------------------------------------------------------------------

/// Strip a leading range off `text`, at the character level rather than
/// the token level.
///
/// Vim's range syntax is conventionally *glued* to what follows with no
/// space (`:5d`, `:'<,'>d`) — [`tokenize`] would read `'<,'>tag` as one
/// word, never as a range plus a verb, if range-stripping ran after
/// tokenizing. Running first, on the raw text, handles the glued form and
/// the spaced form (`:'<,'> tag`) the same way, since whatever whitespace
/// follows the range is simply left for [`tokenize`] to skip as it always
/// does.
fn strip_range(text: &str) -> Result<(Option<Range>, &str), CommandError> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("'<,'>") {
        return Ok((Some(Range::Selection), rest));
    }
    if trimmed.starts_with('\'') {
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        return Err(CommandError::MalformedRange {
            text: trimmed[..end].to_owned(),
        });
    }
    if let Some(rest) = trimmed.strip_prefix('%') {
        return Ok((Some(Range::All), rest));
    }
    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digit_end > 0 {
        // A digit run long enough to overflow `u32` (ten-plus digits) is
        // exactly the held-down-key case `MAX_COUNT`'s own docs describe,
        // not a reason to give up on parsing it as a range at all — falling
        // through to "no range" on overflow would leave a run of digits
        // sitting in front of the verb, which then fails as a whole
        // `UnknownVerb` instead of saturating the way a shorter overflow
        // already does. `u64` comfortably holds any digit run a human
        // could type or a stuck key could produce; if even that overflows,
        // `map_or` saturates the same as an ordinary `u32` overflow would.
        let count = trimmed[..digit_end].parse::<u64>().map_or(MAX_COUNT, |n| {
            u32::try_from(n).unwrap_or(u32::MAX).min(MAX_COUNT)
        });
        return Ok((Some(Range::Count(count)), &trimmed[digit_end..]));
    }
    Ok((None, trimmed))
}

/// Strip a trailing `!` off `text`, at the character level rather than the
/// token level — like [`strip_range`], this has to run before [`tokenize`]
/// discards the difference between quoted and bare text. A `!` is only a
/// bang when it is truly the last character on the line; one sitting
/// inside a `"..."` quote (`ask "what happened!"`) is the argument's own
/// text, not a bang, and quoting is the only way a user has to say so.
/// Stripping `!` off the last already-*tokenized* word instead would look
/// equivalent but is not: by the time a word exists, the quote marks that
/// would distinguish those two cases are already gone.
fn strip_bang(text: &str) -> (bool, &str) {
    let trimmed = text.trim_end();
    match trimmed.strip_suffix('!') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    }
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

/// Parse one `:` line into a [`Resolution`].
///
/// # Errors
///
/// [`CommandError`] naming what about `text` could not be parsed.
pub fn parse(text: &str) -> Result<Resolution, CommandError> {
    let (range, rest) = strip_range(text)?;
    let (bang, rest) = strip_bang(rest);
    let tokens = tokenize(rest)?;

    let mut words: Vec<String> = Vec::new();
    let mut flags: Vec<ParsedFlag> = Vec::new();
    for token in tokens {
        match token {
            Token::Word(word) => words.push(word),
            Token::Flag { name, value } => flags.push(ParsedFlag { name, value }),
        }
    }
    parse_verb(&words, range, flags, bang)
}

/// The verb-resolving half of [`parse`]: longest-matching-prefix of `words`
/// against the registry (module docs' resolution algorithm, step 3), the
/// rest as positionals.
///
/// Each candidate prefix is dot-expanded ([`split_path`]) before the
/// lookup, so a fully glued `message.archive` (one [`tokenize`] word) and a
/// spaced `message archive` (two) resolve identically — but only while
/// still searching for the verb. Once one is found, the positionals are
/// `words[split..]` unchanged: [`tokenize`]'s own words, dots and all, so
/// `message copy report.pdf` keeps `report.pdf` as one argument rather than
/// splitting it into two.
fn parse_verb(
    words: &[String],
    range: Option<Range>,
    flags: Vec<ParsedFlag>,
    bang: bool,
) -> Result<Resolution, CommandError> {
    if words.is_empty() {
        return Err(CommandError::Empty);
    }

    // Longest prefix that is an exact verb wins — `tag rules set` beats
    // `tag` plus two positionals, because a longer real verb is always a
    // more specific match than treating its own trailing segments as
    // arguments.
    for split in (1..=words.len()).rev() {
        let flat: Vec<&str> = words[..split].iter().flat_map(|w| split_path(w)).collect();
        if let Some(verb) = verb_at(&flat) {
            let positionals = words[split..].to_vec();
            check_flags(verb, &flags)?;
            check_positionals(verb, &positionals)?;
            return Ok(Resolution::Invocation(Box::new(Invocation {
                range,
                verb: verb.path.iter().map(|s| (*s).to_owned()).collect(),
                capability: verb.capability,
                action: verb.action,
                positionals,
                flags,
                bang,
            })));
        }
    }

    let flat: Vec<&str> = words.iter().flat_map(|w| split_path(w)).collect();
    let children = children_of(&flat);
    if !children.is_empty() {
        return Ok(Resolution::Children {
            path: words.to_vec(),
            children,
        });
    }

    Err(CommandError::UnknownVerb {
        path: words.join(" "),
        suggestion: closest(&words.join(" ")),
    })
}

fn check_flags(verb: &Verb, flags: &[ParsedFlag]) -> Result<(), CommandError> {
    for flag in flags {
        let Some(declared) = verb.flags.iter().find(|f| f.name == flag.name) else {
            return Err(CommandError::UnknownFlag {
                verb: verb.canonical(),
                flag: flag.name.clone(),
                valid: verb.flags.iter().map(|f| format!("--{}", f.name)).collect(),
            });
        };
        if declared.takes_value && flag.value.is_none() {
            return Err(CommandError::MissingFlagValue {
                flag: flag.name.clone(),
            });
        }
    }
    Ok(())
}

fn check_positionals(verb: &Verb, positionals: &[String]) -> Result<(), CommandError> {
    for (idx, declared) in verb.positionals.iter().enumerate() {
        if declared.required && positionals.get(idx).is_none() {
            return Err(CommandError::MissingPositional {
                verb: verb.canonical(),
                name: declared.name,
            });
        }
    }
    Ok(())
}

/// The registry's closest canonical path to `attempted`, ranked the way
/// `overlays::command_matches` (task 85) ranks the command palette — reusing
/// "is this roughly what was meant" rather than this module inventing a
/// second notion of fuzzy closeness: a prefix match beats a substring match
/// beats "every character of `attempted`, in order, somewhere in the
/// candidate," checked in that one direction only. The reverse direction
/// (is the *candidate* a subsequence of what was typed) looks like it
/// would make closeness usefully symmetric, but does the opposite: it lets
/// a short, unrelated verb like `search` "match" a garbled long one
/// (`message archiv`, missing only its final `e`, would suggest `search`
/// instead of `message archive`) — a short word's letters are easy to find
/// scattered through almost any longer typo, which is exactly backwards
/// from what a suggestion should optimize for. `None` when nothing is
/// close enough to be worth naming over just listing every candidate (a
/// task 89 caller's fallback).
fn closest(attempted: &str) -> Option<String> {
    let needle = attempted.to_ascii_lowercase();
    let mut scored: Vec<(u8, String)> = Vec::new();
    for verb in registry() {
        let candidate = verb.canonical();
        let candidate_lower = candidate.to_ascii_lowercase();
        let tier = if candidate_lower.starts_with(&needle) {
            0
        } else if candidate_lower.contains(&needle) {
            1
        } else if is_subsequence(&needle, &candidate_lower) {
            2
        } else {
            continue;
        };
        scored.push((tier, candidate));
    }
    scored
        .into_iter()
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())))
        .map(|(_, candidate)| candidate)
}

/// Whether every character of `needle`, in order, appears somewhere in
/// `haystack`.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle.chars().all(|c| haystack.by_ref().any(|h| h == c))
}

// ---------------------------------------------------------------------------
// complete
// ---------------------------------------------------------------------------

/// One completion candidate: what the WhichKey band (task 91) renders for
/// the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The text this candidate would insert.
    pub text: String,
    /// Whether choosing this candidate still leaves more to type (`true`
    /// for a verb-path segment with children of its own, `false` for a
    /// leaf verb or a flag) — task 91's "leaves first, groups last"
    /// ordering rule reads this the same way it reads task 91's own
    /// `Continuation::Group` vs. `::Leaf` (not yet written).
    pub has_more: bool,
}

/// Every candidate for what comes next after `text`, positionally: the
/// verb registry while a path is still being typed, then that verb's own
/// flags once the path resolves — module docs' completion table.
/// Positional *values* (a folder name, a tag) are not this module's job;
/// it has no daemon connection and no model state to offer them from,
/// which is task 89's `Model`-backed completion, layered on top of this
/// for the columns this function cannot fill in.
#[must_use]
pub fn complete(text: &str) -> Vec<Candidate> {
    let Ok((_, rest)) = strip_range(text) else {
        return Vec::new();
    };
    let Ok(tokens) = tokenize(rest) else {
        return Vec::new();
    };
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) => Some(w.clone()),
            Token::Flag { .. } => None,
        })
        .collect();

    // A trailing `.` finishes a segment exactly the way a trailing space
    // does (module docs' "one transform") — `complete("message.")` has to
    // offer `message`'s children, not re-suggest `message` itself, which a
    // check for trailing *whitespace* alone would do (the word `tokenize`
    // hands back is literally `"message."`, dot included — `tokenize`
    // itself is deliberately not `.`-aware; see its own docs).
    let ends_with_separator =
        rest.ends_with(char::is_whitespace) || rest.ends_with('.') || rest.is_empty();

    // Every settled word is dot-expanded the same way `parse_verb` matches
    // a verb path — one glued word can name several segments. The
    // in-progress last word, when the line has not just ended a segment,
    // gets the same treatment for everything but its own final piece,
    // which is the partial filter still being typed.
    let mut prefix: Vec<&str> = Vec::new();
    let partial: &str = if ends_with_separator {
        for word in &words {
            prefix.extend(split_path(word));
        }
        ""
    } else {
        match words.split_last() {
            Some((last, settled)) => {
                for word in settled {
                    prefix.extend(split_path(word));
                }
                let mut last_segments = split_path(last);
                let partial = last_segments.pop().unwrap_or("");
                prefix.extend(last_segments);
                partial
            }
            None => "",
        }
    };

    let children = children_of(&prefix);
    let mut seen_next_segments = BTreeSet::new();
    let mut out = Vec::new();
    for verb in &children {
        let next = verb.path[prefix.len()];
        if !next.starts_with(partial) || !seen_next_segments.insert(next) {
            continue;
        }
        // Whether *any* verb sharing this next segment goes deeper, not
        // just the one that happened to be first: `search` and
        // `search.explain` both real, both auto-derived, and registry
        // order (`Action::ALL`'s) puts the leaf first — computing this
        // from `verb` alone would report `search` as childless.
        let has_more = children
            .iter()
            .any(|v| v.path[prefix.len()] == next && v.path.len() > prefix.len() + 1);
        out.push(Candidate {
            text: next.to_owned(),
            has_more,
        });
    }
    // An exact verb at `prefix` with no completed word yet offers its own
    // flags — `:tag add ` (trailing space) should suggest `--sync`, not
    // repeat `add`.
    if partial.is_empty() {
        if let Some(verb) = verb_at(&prefix) {
            out.extend(flag_candidates(verb));
        }
    }
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

/// The `--flag` candidates [`complete`] offers once a verb's path is fully
/// typed with nothing yet started for the next word — pulled out of
/// [`complete`] so it can be tested directly the same way
/// [`check_flags`]/[`check_positionals`] are: a [`Verb`] cannot be
/// registered into the process-wide registry from a test, and no real verb
/// declares any flags yet ([`explicit`]'s docs).
fn flag_candidates(verb: &Verb) -> Vec<Candidate> {
    verb.flags
        .iter()
        .map(|flag| Candidate {
            text: format!("--{}", flag.name),
            has_more: flag.takes_value,
        })
        .collect()
}
