//! The content, export and analytics verbs (task 99): what is in the mail, what
//! the mail says about the people sending it, and how to get it out.
//!
//! # Four families, one filter
//!
//! Thirty-six verbs across eight services is too many for one table, so this
//! module is a dispatcher over four: [`analytics`] (what the mailbox says about
//! itself, plus the export), [`attach`] (what is attached, and searching inside
//! it), [`extract`] (what a message contains — events, tasks, structured data,
//! links — plus the three search verbs that read the index rather than the mail),
//! and [`library`] (the things a user names and keeps: notes, saved searches,
//! smart folders).
//!
//! All four live under `tui::commands::content`, which is the acceptance's own
//! verify filter — so it selects every one of them.
//!
//! # A window is a duration here and an instant on the wire
//!
//! `--since 30d` is what `mail stats` accepts and what somebody types; the RPCs
//! take absolute unix seconds, because a report has to name the window it
//! summarized and a relative bound would mean something different by the time it
//! was read. The conversion needs the clock, and `update` is pure — so the
//! [`Cmd`] carries the duration and the wire seam subtracts it from `now`. That
//! is the same split `Msg::Tick` exists for: the model never reads a clock.

#[cfg(test)]
mod tests;

pub mod analytics;
pub mod attach;
pub mod extract;
pub mod library;

use rmail_core::command::Invocation;

use super::{Answer, Target};

/// The content verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    analytics::answer(invocation, target, generation)
        .or_else(|| attach::answer(invocation, target, generation))
        .or_else(|| extract::answer(invocation, target, generation))
        .or_else(|| library::answer(invocation, target, generation))
}

/// How far back a `--since` duration reaches, in seconds.
///
/// A duration, not a timestamp: `30d` reads better than a unix second and is
/// what every other duration-shaped flag in this vocabulary accepts. Converted
/// at the wire seam rather than here — see the module docs.
///
/// # Errors
///
/// A message naming the offending value.
pub(super) fn since(invocation: &Invocation) -> Result<Option<i64>, String> {
    let Some(text) = super::flag(invocation, "since") else {
        return Ok(None);
    };
    let duration = rmail_core::config::parse_human_duration(text)
        .map_err(|error| format!("--since {text:?}: {error}"))?;
    let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    if secs <= 0 {
        // A zero-length window summarizes nothing, and reporting on nothing as
        // though it were a report is worse than refusing.
        return Err(format!("--since {text:?}: a window has to have a length"));
    }
    Ok(Some(secs))
}

/// Where a window ends, as unix seconds. `None` means now.
///
/// Absolute, unlike `--since`: "until 30 days ago" is a window nobody asks for,
/// and the flag exists for re-running a report over a period that has already
/// closed — which is a fixed instant by definition.
///
/// # Errors
///
/// A message naming the offending value.
pub(super) fn until(invocation: &Invocation) -> Result<Option<i64>, String> {
    let Some(text) = super::flag(invocation, "until") else {
        return Ok(None);
    };
    text.parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| format!("--until {text:?}: unix seconds, as the reports print them"))
}

/// A whole-number flag, if the line carried one.
///
/// # Errors
///
/// A message naming the offending flag and value.
pub(super) fn count(invocation: &Invocation, name: &str) -> Result<Option<i64>, String> {
    match super::flag(invocation, name) {
        None => Ok(None),
        Some(text) => text
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .map(Some)
            .ok_or_else(|| format!("--{name} {text:?}: a whole number, at least zero")),
    }
}

/// The message a verb acts on: the id it was given, or the one on screen.
///
/// **Only for a verb whose first declared positional is a message id.** A verb
/// taking free text — `:attach ask <question>`, `:note add <body>` — must use
/// [`on_screen`] instead, or the first word of what somebody wrote is read as an
/// id and the whole line is refused for not being a number.
///
/// # Errors
///
/// The refusal to show when there is neither.
pub(super) fn message(invocation: &Invocation, target: &Target) -> Result<i64, String> {
    if let Some(id) = invocation.positionals.first() {
        return id
            .parse()
            .map_err(|_| format!("{id:?} is not a message id"));
    }
    on_screen(target)
}

/// The message on screen, for a verb whose positionals are not an id.
///
/// # Errors
///
/// The refusal to show when there is none.
pub(super) fn on_screen(target: &Target) -> Result<i64, String> {
    target
        .message_id
        .ok_or_else(|| "no message selected".to_owned())
}

/// The account a verb acts on.
///
/// Every verb in this family is about one account's mail, so there is no
/// `--account` and no zero-means-everything: it is the account on screen, and a
/// session with none loaded has nothing for these to report on.
///
/// # Errors
///
/// The refusal to show when no account has loaded.
pub(super) fn account(target: &Target) -> Result<i64, String> {
    match target.account_id {
        0 => Err("no account loaded yet".to_owned()),
        account_id => Ok(account_id),
    }
}
