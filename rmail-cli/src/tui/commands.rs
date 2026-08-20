//! What each daemon verb answers with (task 94 onward).
//!
//! # One table, not a dispatcher
//!
//! Everything here is data: given a verb, the [`Cmd`] it issues, and — for a
//! verb that answers with rows — the title and columns its Report opens with.
//! Nothing in this module touches `Model`, opens an overlay or issues anything.
//! `tui::model`'s `run_daemon_command` is the single place that does, so the
//! twenty-odd verbs below share one implementation of the confirmation gate,
//! the generation stamp, the Report and the status line.
//!
//! That split is what makes the table testable. "`:index rebuild` refuses
//! without a bang", "`:ai cost` and `:ai status` are two views of one RPC",
//! "every verb the registry declares is answered here" — each is a claim about
//! this function and can be checked without a daemon, a terminal or a `Model`.
//!
//! # Rows or a fact
//!
//! A verb answering with more than one number opens a Report ([`Answer::Rows`]);
//! one answering with a single fact says so on the status line
//! ([`Answer::Fact`]). `:index gc` reclaimed seven different things and wants a
//! table; `:ai resume` either resumed or did not.
//!
//! The line is not "does it mutate". `:sync now` mutates and answers with a row
//! per folder, and reducing that to "synced" would throw away the one thing
//! somebody ran it to see.
//!
//! # Confirmation
//!
//! [`Request::confirm`] is per verb and deliberately not derived from
//! `parity::Command::effect`. Task 89 settled that a `:` line typed in full is
//! already the deliberate act a confirmation asks for, so gating every mutating
//! verb would make the question meaningless by asking it twenty times. What is
//! left is a judgement about *individual* verbs that are expensive and hard to
//! undo, which is a different question from "does this change state" — and it
//! rides on the spec rather than in a second table, so a verb cannot be added
//! without the author looking at the field.

pub mod daemon;
pub mod rule;
pub mod tag;

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::model::Cmd;
use super::report::ReportColumn;

/// Which pass `IndexService.Reindex` is being asked for.
///
/// The proto has four modes and the CLI spells each as its own verb; the TUI
/// takes the two that need no arguments beyond what is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reindex {
    /// `:index run` — drain whatever is already queued.
    Drain,
    /// `:index reindex` — re-enqueue the folder on screen.
    Selection,
}

/// Which way a pause verb goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// Stop.
    Stop,
    /// Start again.
    Start,
}

impl Pause {
    /// The `paused` flag this sends.
    #[must_use]
    pub const fn paused(self) -> bool {
        matches!(self, Self::Stop)
    }
}

/// What the model knows that a verb might need.
///
/// Passed in rather than read here, because this module has no `Model` — which
/// is the property that makes every claim below checkable without building one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The account on screen, or 0 when none has loaded.
    pub account_id: i64,
    /// The folder whose rows are listed, if one is open.
    pub mailbox_id: Option<i64>,
    /// The message the viewer or the list cursor is on, if any.
    pub message_id: Option<i64>,
    /// The messages a range applies to: the visual selection when there is one,
    /// otherwise just [`Target::message_id`].
    ///
    /// `model::targets`' own answer, passed in rather than re-derived — which is
    /// what makes `:'<,'>tag add work` need no code of its own. Task 89's rule is
    /// that a `:` line carrying `'<,'>` does what the key does with the same
    /// selection up, and a second notion of "these messages" here is exactly how
    /// the two would drift.
    pub selection: Vec<i64>,
    /// The TOML `:rule new` last drafted, if it has drafted one this session.
    ///
    /// `:rule add`'s whole argument — see `rule`'s module docs on why a rule is
    /// stored from a draft rather than typed or read from a file.
    pub rule_draft: Option<String>,
}

/// What a verb answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Rows, in a Report.
    Rows(Box<Request>),
    /// A single fact, on the status line.
    Fact(Box<Request>),
    /// The verb needs something the screen does not have — no account loaded,
    /// no message under the cursor. Carries what to say about it.
    ///
    /// A refusal rather than a request with a zero in it: `AnalyzeMessage` on
    /// message 0 is `INVALID_ARGUMENT` from the daemon a round trip later, and
    /// "no message selected" said immediately is the same answer sooner.
    Refused(String),
}

/// One verb's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What to issue.
    pub cmd: Cmd,
    /// The Report's border, or — for a fact — what the status line says while
    /// the request is outstanding.
    pub title: String,
    /// The Report's columns. Empty for a fact.
    pub columns: Vec<ReportColumn>,
    /// What to ask before running it, when this verb is one that asks.
    ///
    /// `None` for almost everything; see the module docs on why this is a
    /// per-verb judgement and not `effect()`.
    pub confirm: Option<String>,
}

impl Request {
    /// A verb that answers with rows.
    pub(super) fn rows(cmd: Cmd, title: &str, columns: Vec<ReportColumn>) -> Answer {
        Answer::Rows(Box::new(Self {
            cmd,
            title: title.to_owned(),
            columns,
            confirm: None,
        }))
    }

    /// A verb that answers with a fact.
    pub(super) fn fact(cmd: Cmd, title: &str) -> Answer {
        Answer::Fact(Box::new(Self {
            cmd,
            title: title.to_owned(),
            columns: Vec::new(),
            confirm: None,
        }))
    }
}

/// What a verb needing an account says when none has loaded.
///
/// A named refusal rather than a `None` from [`answer`], because the two mean
/// different things to the caller: `None` is "this build has no answer for that
/// verb", and reporting a missing account as that would send somebody looking
/// for a feature that is present and simply has nothing to act on yet. Named
/// once rather than written at each call site, so they cannot disagree about how
/// to say it.
pub(super) fn no_account() -> Answer {
    Answer::Refused("no account loaded yet".to_owned())
}

/// The account on screen, if one has loaded.
///
/// Zero means none: every account id in this API is a positive row id, and the
/// proto spells `0` as "every account" for the RPCs that accept it — which is a
/// different question from the one a folder listing or a tag table is asking.
pub(super) fn account(target: &Target) -> Option<i64> {
    (target.account_id != 0).then_some(target.account_id)
}

/// The `n`th positional, if it was given.
pub(super) fn nth(invocation: &Invocation, index: usize) -> Option<String> {
    invocation.positionals.get(index).cloned()
}

/// The first positional, if it was given.
pub(super) fn first(invocation: &Invocation) -> Option<String> {
    nth(invocation, 0)
}

/// Every positional, joined with spaces.
///
/// What a verb taking free text reads: an unquoted sentence is what somebody
/// types, and using only its first word is the silent truncation `:helpgrep`'s
/// own docs call out. Empty when nothing was given.
pub(super) fn joined(invocation: &Invocation) -> String {
    invocation.positionals.join(" ")
}

/// A value-taking flag's value, if it was given.
pub(super) fn flag(invocation: &Invocation, name: &str) -> Option<String> {
    invocation
        .flags
        .iter()
        .find(|flag| flag.name == name)
        .and_then(|flag| flag.value.clone())
}

/// Whether a switch flag was given.
pub(super) fn switch(invocation: &Invocation, name: &str) -> bool {
    invocation.flags.iter().any(|flag| flag.name == name)
}

/// How many days a `--days` flag asked for, or `None` for the daemon's default.
///
/// Refused rather than clamped when it is not a number: a backtest over zero days
/// because `--days seven` did not parse is an answer about nothing, presented as
/// an answer about something.
pub(super) fn days(invocation: &Invocation) -> Result<Option<u32>, String> {
    match flag(invocation, "days") {
        None => Ok(None),
        Some(text) => text
            .parse::<u32>()
            .map(Some)
            .map_err(|_| format!("--days {text:?}: a whole number of days")),
    }
}

/// What a verb answers with, asked of each domain in turn.
///
/// `None` for a verb no domain answers for — which is a real state, not an
/// oversight: the registry declares verbs for tasks 96 onward, and `tui::model`
/// reports "no answer for it" rather than pretending.
///
/// The domains are tried in order and the first answer wins. They cannot
/// overlap: every arm matches on a whole verb path, and
/// `command::tests::no_two_real_verbs_share_the_same_path` is what makes a path
/// unambiguous in the first place — so "first wins" is a statement about
/// evaluation order, not a precedence rule anybody has to remember.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    daemon::answer(invocation, target, generation)
        .or_else(|| tag::answer(invocation, target, generation))
        .or_else(|| rule::answer(invocation, target, generation))
}
