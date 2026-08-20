//! The rule verbs (task 95). `RuleService`'s first human surface anywhere, so
//! these spellings are what a future `mail rule` has to adopt.
//!
//! # Why `:rule add` takes no argument
//!
//! `CreateRule` takes a TOML document, and a one-line command grammar cannot
//! carry one: a rule is several keys and at least one multi-line predicate, and a
//! quoted positional with `\n` in it is not something anybody types. Three ways
//! out were considered.
//!
//! A file path (`:rule add rules/newsletters.toml`) reads well and is what the
//! CLI form will be, but it puts a user-supplied path and a blocking read into
//! the TUI's executor for the sake of one verb — the same trap the command
//! history had to be careful about — and it is a fourth way to say "a rule" that
//! no other verb here has.
//!
//! An `Input` overlay collecting one line has the same problem as the positional.
//!
//! What is implemented is the flow somebody actually wants: {{`:rule new`}}
//! drafts a rule *from words*, shows its TOML and a dry run over real mail, and
//! `:rule add` stores that draft. Two commands, no filesystem, and the dangerous
//! half — a rule that will act on mail — is only reachable after its dry run has
//! been on screen. A hand-authored TOML file still goes in through
//! `mail api call RuleService.CreateRule`, which is real and is the same call.
//!
//! # A dry run is not a backtest
//!
//! `:rule run` evaluates against messages *now* and applies nothing;
//! `:rule backtest` replays a rule over the last N days and reports what it
//! would have done. Both report `MessageOutcome` rows, and keeping them separate
//! matters because only one of them is bounded by what is on screen.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{account, days, first, flag, joined, no_account, switch, Answer, Request, Target};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// The columns every `MessageOutcome` table draws.
///
/// One shape for the dry run, the synthesis preview and the backtest, because all
/// three answer with the same message: three layouts over one type would be three
/// chances to disagree about what a column means.
fn outcome_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("message", 9),
        ReportColumn::new("from", 22),
        ReportColumn::new("subject", 30),
        ReportColumn::new("rules", 22),
    ]
}

/// The rule verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "rule list" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::RuleList {
                    generation,
                    account_id,
                },
                "rules",
                vec![
                    ReportColumn::new("rule", 26),
                    ReportColumn::new("state", 10),
                    ReportColumn::new("updated", 18),
                ],
            )
        }
        "rule new" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let instruction = joined(invocation);
            if instruction.is_empty() {
                return Some(Answer::Refused(
                    "say what the rule should do — :rule new archive newsletters".to_owned(),
                ));
            }
            let days = match days(invocation) {
                Ok(days) => days,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::RuleSynthesize {
                    generation,
                    account_id,
                    instruction: instruction.clone(),
                    days,
                },
                "rule new — the draft, and what it would have done",
                outcome_columns(),
            )
        }
        "rule add" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            // The draft `:rule new` left behind. Refused rather than sent empty:
            // `CreateRule` on an empty document is INVALID_ARGUMENT a round trip
            // later, and "draft one first" said now is the same answer sooner.
            let Some(toml) = target.rule_draft.clone() else {
                return Some(Answer::Refused(
                    "no draft — write one with :rule new <what it should do>".to_owned(),
                ));
            };
            Request::fact(
                Cmd::RuleCreate { account_id, toml },
                "storing the drafted rule…",
            )
        }
        "rule run" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let message_ids = target.selection.clone();
            if message_ids.is_empty() {
                return Some(Answer::Refused("no message selected".to_owned()));
            }
            Request::rows(
                Cmd::RuleEvaluate {
                    generation,
                    account_id,
                    message_ids,
                    // One rule by name, or every enabled one. Named rather than
                    // positional because the common case is "all of them" and a
                    // positional would make the common case the odd one.
                    rule: flag(invocation, "rule"),
                },
                "rule run — a dry run over the selection",
                outcome_columns(),
            )
        }
        "rule backtest" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused("name a rule to backtest".to_owned()));
            };
            let days = match days(invocation) {
                Ok(days) => days,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::RuleBacktest {
                    generation,
                    account_id,
                    name: name.clone(),
                    days,
                },
                &format!("rule backtest {name}"),
                outcome_columns(),
            )
        }
        "rule correct" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            let prompt = joined(invocation);
            if prompt.is_empty() {
                return Some(Answer::Refused(
                    "quote the criterion you are correcting — :rule correct \"is a newsletter\" --no"
                        .to_owned(),
                ));
            }
            Request::fact(
                Cmd::RuleCorrect {
                    account_id,
                    message_id,
                    prompt: prompt.clone(),
                    // Absent means "yes, this message is what that criterion
                    // says"; `--no` is the other answer. Those are the only two
                    // `RecordCorrection` records, so a switch is the whole
                    // vocabulary rather than a narrowing of it.
                    expected: !switch(invocation, "no"),
                },
                "recording the correction…",
            )
        }
        _ => return None,
    })
}
