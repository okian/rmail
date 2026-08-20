//! The tag verbs (task 95): applying tags, listing them, the AI suggestion
//! stream, and the rules that let a confident suggestion apply itself.
//!
//! # Ranges are the selection, and nothing else
//!
//! `:'<,'>tag add work` is the one range this grammar honours, and it needs no
//! code of its own beyond passing the ids along: task 89's rule is that a `:`
//! line carrying `'<,'>` does what the key does with the same selection up, and
//! `Target::selection` is that selection. What is *not* here is a second way to
//! address messages — `:tag bulk` takes a query because `BulkTag` does, and that
//! is a different question from "these rows".
//!
//! # `:tag bulk` is not `:'<,'>tag add`
//!
//! They look interchangeable and are not. `AddTag` applies to messages this
//! client has loaded and can name; `BulkTag` applies to everything a *query*
//! selects, in one transaction, including mail the client has never seen. The
//! CLI keeps them apart for the same reason and refuses to let `mail tag` reach
//! the bulk form at all (`parity`'s own note on `TagBulkTag` says so); the TUI
//! keeps both and spells them differently.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{account, first, flag, no_account, nth, switch, Answer, Request, Target};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// What `:tag rules set` stores as the rule's mode.
///
/// Named rather than passed as the proto's `i32`, so the model stays comparable
/// with `assert_eq!` and a bad `--mode` is refused where it was typed instead of
/// a round trip later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMode {
    /// A confident suggestion still waits to be accepted. The safe default.
    Suggest,
    /// A confident suggestion applies itself.
    Auto,
}

impl RuleMode {
    /// The mode `text` names, or `None` when it names neither.
    ///
    /// Only two, because `TagRuleMode` only has two: `UNSPECIFIED` is a wire
    /// artefact rather than a mode, and accepting it here would store a rule
    /// whose behaviour is whatever the daemon defaults to.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "suggest" => Some(Self::Suggest),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// What `:tag accept` and `:tag reject` do to a pending suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    /// Apply it.
    Accept,
    /// Discard it.
    Reject,
}

impl Resolve {
    /// The `accept` flag this sends.
    #[must_use]
    pub const fn accept(self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// The tag verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "tag list" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::TagList {
                    generation,
                    account_id,
                },
                "tags",
                vec![
                    ReportColumn::new("tag", 26),
                    ReportColumn::new("messages", 10),
                    ReportColumn::new("sync", 8),
                    ReportColumn::new("colour", 10),
                ],
            )
        }
        "tag add" | "tag rm" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused("name a tag".to_owned()));
            };
            let ids = target.selection.clone();
            if ids.is_empty() {
                return Some(Answer::Refused("no message selected".to_owned()));
            }
            // A row per message, because a bulk apply that answered "done" would
            // hide the one outcome worth seeing: a tag that applied to four of
            // five messages and failed on the fifth.
            Request::rows(
                Cmd::TagApply {
                    generation,
                    message_ids: ids,
                    name: name.clone(),
                    remove: verb == "tag rm",
                },
                &format!("{verb} {name}"),
                vec![
                    ReportColumn::new("message", 10),
                    ReportColumn::new("tag", 22),
                    ReportColumn::new("outcome", 24),
                ],
            )
        }
        "tag new" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused("name the tag to create".to_owned()));
            };
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let sync = match flag(invocation, "sync") {
                None => None,
                Some(text) => match Sync::parse(&text) {
                    Some(sync) => Some(sync),
                    None => {
                        return Some(Answer::Refused(format!(
                            "--sync {text:?}: one of local, imap, auto"
                        )))
                    }
                },
            };
            Request::fact(
                Cmd::TagCreate {
                    account_id,
                    name: name.clone(),
                    color: flag(invocation, "color"),
                    sync,
                },
                &format!("creating the tag {name}…"),
            )
        }
        "tag bulk" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            // Two positionals in a fixed order, because the query is the thing
            // most likely to contain spaces and a trailing tag is what somebody
            // types: `:tag bulk "from:stripe is:unread" invoices`.
            let (Some(query), Some(name)) = (nth(invocation, 0), nth(invocation, 1)) else {
                return Some(Answer::Refused(
                    "give a query and a tag — :tag bulk \"from:stripe\" invoices".to_owned(),
                ));
            };
            Request::rows(
                Cmd::TagBulk {
                    generation,
                    account_id,
                    query: query.clone(),
                    name: name.clone(),
                },
                &format!("tag bulk {name}"),
                vec![
                    ReportColumn::new("what", 22),
                    ReportColumn::new("count", 12),
                ],
            )
        }
        "tag suggest" => {
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            Request::rows(
                Cmd::TagSuggest {
                    generation,
                    message_id,
                },
                "tag suggest — Enter accepts · n rejects",
                vec![
                    ReportColumn::new("tag", 20),
                    ReportColumn::new("confidence", 11),
                    ReportColumn::new("why", 40),
                ],
            )
        }
        "tag accept" | "tag reject" => {
            let resolve = if verb == "tag accept" {
                Resolve::Accept
            } else {
                Resolve::Reject
            };
            let Some(id) = first(invocation).and_then(|text| text.parse::<i64>().ok()) else {
                return Some(Answer::Refused(
                    "give a suggestion id, as :tag suggest shows them".to_owned(),
                ));
            };
            Request::fact(
                Cmd::TagResolve {
                    message_tag_id: id,
                    resolve,
                },
                match resolve {
                    Resolve::Accept => "accepting the suggestion…",
                    Resolve::Reject => "rejecting the suggestion…",
                },
            )
        }
        "tag rules" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::TagRules {
                    generation,
                    account_id,
                },
                "tag rules — what a confident suggestion may do by itself",
                vec![
                    ReportColumn::new("rule", 22),
                    ReportColumn::new("tag", 18),
                    ReportColumn::new("mode", 9),
                    ReportColumn::new("min conf", 9),
                    ReportColumn::new("state", 9),
                ],
            )
        }
        "tag rules set" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let (Some(name), Some(tag)) = (nth(invocation, 0), nth(invocation, 1)) else {
                return Some(Answer::Refused(
                    "give the rule's name and the tag it applies".to_owned(),
                ));
            };
            // `suggest` when nothing says otherwise, which is the safe half of
            // the pair: without a rule at `auto`, every suggestion waits to be
            // accepted, and the proto's own docs call that the safe default
            // rather than an oversight.
            let mode = match flag(invocation, "mode") {
                None => RuleMode::Suggest,
                Some(text) => match RuleMode::parse(&text) {
                    Some(mode) => mode,
                    None => {
                        return Some(Answer::Refused(format!(
                            "--mode {text:?}: one of suggest, auto"
                        )))
                    }
                },
            };
            let min_conf = match flag(invocation, "min-conf") {
                None => DEFAULT_MIN_CONF,
                Some(text) => match text.parse::<f64>() {
                    Ok(value) if (0.0..=1.0).contains(&value) => value,
                    _ => {
                        return Some(Answer::Refused(format!(
                            "--min-conf {text:?}: a number between 0 and 1"
                        )))
                    }
                },
            };
            Request::fact(
                Cmd::TagRuleSet {
                    account_id,
                    name: name.clone(),
                    tag: tag.clone(),
                    mode,
                    min_conf_pct: percent(min_conf),
                    enabled: !switch(invocation, "disabled"),
                },
                &format!("storing the rule {name}…"),
            )
        }
        _ => return None,
    })
}

/// The confidence a rule needs when nothing says otherwise.
///
/// High, deliberately: this is the threshold above which a model's guess is
/// allowed to change the mailbox without anybody looking, and a default low
/// enough to be convenient is a default that mis-tags mail.
const DEFAULT_MIN_CONF: f64 = 0.9;

/// A tag's IMAP sync mode, as `--sync` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    /// Never leaves this machine.
    Local,
    /// Stored as an IMAP keyword.
    Imap,
    /// Whatever the server supports.
    Auto,
}

impl Sync {
    /// The mode `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "local" => Some(Self::Local),
            "imap" => Some(Self::Imap),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// A confidence as whole percent.
///
/// Carried as an integer rather than the `f64` the proto takes, because `Model`
/// is compared with `assert_eq!` throughout its tests and a float in a `Cmd`
/// would cost `Eq` on every enum that reaches it — the same trade
/// `overlays::Explanation` makes. Percent rather than basis points because that
/// is the precision a person types.
fn percent(value: f64) -> u32 {
    let scaled = (value * 100.0).round();
    if scaled <= 0.0 {
        return 0;
    }
    if scaled >= 100.0 {
        return 100;
    }
    // In range and finite by the two guards above, so the cast is exact.
    scaled as u32
}
