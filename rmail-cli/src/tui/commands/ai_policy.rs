//! The AI policy, safety and audit verbs (task 96): what a model call is allowed
//! to cost, which backend serves it, whether a message is trying to steer it, and
//! what it actually did.
//!
//! # The one verb here that opens a form
//!
//! `SetBudget` replaces a scope's whole budget: a cap the request omits is a cap
//! cleared, which the proto and `mail ai budget set` both say outright. So
//! `:ai budget set --daily-hard-usd 5` cannot simply be sent — against a budget
//! that already had a monthly cap it would delete it. The bare verb therefore
//! reads the current caps and opens a form pre-filled with them, and any flags
//! the line carried pre-fill it further; a trailing `!` sends what was typed with
//! the CLI's replace-semantics, for somebody who has already decided.
//!
//! `tui::form`'s module docs carry the rest of that reasoning, including why
//! applying a form is a `:` line rather than a private path to the daemon.
//!
//! # Two verbs the acceptance's list does not name
//!
//! `:ai confirm` reaches `ConfirmInjection`, and `:ai audit --all` reaches
//! `ExportLedger`. Both are RPCs the acceptance counts (`AiSafetyService` (2),
//! `AuditService` (2)) and neither has a spelling in its list, so they take the
//! CLI's: `mail ai scan-injection --confirm` and the ledger export. A scan
//! report's rows carry the confirm invocation, which is why it needs to be a verb
//! at all — a row's action *is* an `Invocation` (task 90).

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{first, flag, switch, Answer, Request, Target};
use crate::tui::form::Field;
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// Which sub-budget `:ai budget set` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Every call counts toward it.
    All,
    /// Backlog work only. A bulk call is checked against both.
    Bulk,
}

/// Which backend `:ai provider set` routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic Claude over the network.
    Claude,
    /// On-device inference. Zero egress.
    Local,
    /// Clear the override and inherit again.
    Inherit,
}

impl Provider {
    /// The backend `text` names, or `None`.
    ///
    /// `clear` rather than an empty string for the inherit case, because
    /// `AI_PROVIDER_KIND_UNSPECIFIED` means two different things on the wire —
    /// "clear the override" on a set, "no override stored" in a response — and a
    /// verb that spelled it as absence would be ambiguous with "no flag given".
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "claude" => Some(Self::Claude),
            "local" => Some(Self::Local),
            "clear" | "inherit" => Some(Self::Inherit),
            _ => None,
        }
    }
}

/// Whether `:ai confirm` releases withheld actions or re-withholds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Release the actions a flagged message had withheld.
    Release,
    /// Withhold them again.
    Revoke,
}

impl Confirm {
    /// The `confirmed` flag this sends.
    #[must_use]
    pub const fn confirmed(self) -> bool {
        matches!(self, Self::Release)
    }
}

/// The eight caps `:ai budget set` writes, as the form's fields.
///
/// Declared once, here, rather than at the two places that need them (the form
/// that pre-fills and the request that reads it): a ninth cap added to the proto
/// should be one edit, and a field whose flag the verb does not declare would
/// build a line the parser rejects.
pub const CAPS: [(&str, &str, &str); 8] = [
    (
        "daily-soft-usd",
        "daily soft $",
        "downgrade the model at or above this much spent today",
    ),
    (
        "daily-hard-usd",
        "daily hard $",
        "block dispatch at or above this much spent today",
    ),
    (
        "daily-soft-tokens",
        "daily soft tokens",
        "downgrade the model at or above this many tokens today",
    ),
    (
        "daily-hard-tokens",
        "daily hard tokens",
        "block dispatch at or above this many tokens today",
    ),
    (
        "monthly-soft-usd",
        "monthly soft $",
        "downgrade the model at or above this much this month",
    ),
    (
        "monthly-hard-usd",
        "monthly hard $",
        "block dispatch at or above this much this month",
    ),
    (
        "monthly-soft-tokens",
        "monthly soft tokens",
        "downgrade the model at or above this many tokens this month",
    ),
    (
        "monthly-hard-tokens",
        "monthly hard tokens",
        "block dispatch at or above this many tokens this month",
    ),
];

/// The policy verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "ai budget status" => {
            let account_id = match scope(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::BudgetStatus {
                    generation,
                    account_id,
                },
                &format!(
                    "ai budget — {} spend against the caps in force",
                    scope_label(account_id)
                ),
                budget_columns(),
            )
        }
        "ai budget set" => {
            let account_id = match scope(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let class = if switch(invocation, "bulk") {
                Class::Bulk
            } else {
                Class::All
            };
            let caps = match caps(invocation) {
                Ok(caps) => caps,
                Err(why) => return Some(Answer::Refused(why)),
            };
            if invocation.bang {
                // What was typed, with the RPC's own replace-semantics: every
                // cap not on the line is cleared. The bang is the opting out.
                return Some(Request::fact(
                    Cmd::BudgetSet {
                        account_id,
                        class,
                        caps,
                    },
                    "storing the budget…",
                ));
            }
            // No bang: read what is there and open a form over it, so applying
            // sends the caps in force rather than only the one that was typed.
            // The caps parsed above are not discarded — the form pre-fills from
            // them once the read lands, which is what makes refusing a typo here
            // rather than at the wire seam worth doing.
            Request::form(
                Cmd::BudgetForm {
                    generation,
                    account_id,
                    class,
                },
                &format!(
                    "ai budget set — {} caps for {}",
                    class_label(class),
                    scope_label(account_id)
                ),
                fields(),
            )
        }
        "ai provider status" => {
            let account_id = match scope(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::ProviderStatus {
                    generation,
                    account_id,
                },
                "ai provider — which backend serves a call",
                vec![
                    ReportColumn::new("what", 20),
                    ReportColumn::new("value", 44),
                ],
            )
        }
        "ai provider set" => {
            let account_id = match scope(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(text) = first(invocation) else {
                return Some(Answer::Refused(
                    "name a backend — claude, local, or clear to inherit".to_owned(),
                ));
            };
            let Some(provider) = Provider::parse(&text) else {
                return Some(Answer::Refused(format!(
                    "{text:?}: one of claude, local, clear"
                )));
            };
            Request::fact(
                Cmd::ProviderSet {
                    account_id,
                    provider,
                },
                "routing AI calls…",
            )
        }
        "ai scan" => {
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            Request::rows(
                Cmd::ScanInjection {
                    generation,
                    message_id,
                },
                "ai scan — is this message steering the model",
                injection_columns(),
            )
        }
        "ai confirm" => {
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            let confirm = if switch(invocation, "revoke") {
                Confirm::Revoke
            } else {
                Confirm::Release
            };
            // A report rather than a fact: `ConfirmInjection` answers with the
            // whole flag again, and the point of confirming is seeing what is
            // released — a one-line "done" would hide it.
            Request::rows(
                Cmd::ConfirmInjection {
                    generation,
                    message_id,
                    confirm,
                },
                match confirm {
                    Confirm::Release => "ai confirm — actions released",
                    Confirm::Revoke => "ai confirm — actions withheld again",
                },
                injection_columns(),
            )
        }
        "ai audit" => {
            let account_id = match scope(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AuditQuery {
                    generation,
                    // Zero is "every account" for this filter, which is what a
                    // ledger question usually means — unlike the budget verbs,
                    // where zero is the *global* budget rather than all of them.
                    account_id,
                    model: flag(invocation, "model").map(str::to_owned),
                    failed_only: switch(invocation, "failed"),
                    // The whole ledger rather than the most recent page. Still
                    // capped by the Report's own row limit, which the report says.
                    whole_ledger: switch(invocation, "all"),
                },
                "ai audit — every model call, newest first",
                vec![
                    ReportColumn::new("when", 17),
                    ReportColumn::new("model", 20),
                    ReportColumn::new("pass", 12),
                    ReportColumn::new("tokens", 14),
                    ReportColumn::new("cost", 9),
                    ReportColumn::new("outcome", 18),
                ],
            )
        }
        _ => return None,
    })
}

/// The columns `:ai budget status` draws.
///
/// A row per class, window *and* dimension — eight of them — rather than one row
/// per window with the dollars and the tokens side by side. The row's tone is
/// the point of the report ("am I about to be throttled"), and a row that was
/// over the soft cap on tokens while under it on dollars would have to pick one
/// tone for two different answers.
fn budget_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("class", 12),
        ReportColumn::new("window", 14),
        ReportColumn::new("measure", 8),
        ReportColumn::new("spent", 11),
        ReportColumn::new("soft", 11),
        ReportColumn::new("hard", 11),
        ReportColumn::new("state", 13),
    ]
}

/// [`CAPS`] as a form's fields, with no values in them yet.
///
/// Empty rather than zeroed: an absent cap and a cap of zero are different
/// things on this RPC — zero forbids all spending — and a form that opened with
/// zeros in it would turn "no cap" into "spend nothing" for anyone who applied
/// it without reading.
#[must_use]
pub fn fields() -> Vec<Field> {
    CAPS.into_iter()
        .map(|(flag, label, hint)| Field::new(flag, label, hint, String::new()))
        .collect()
}

/// The columns a scan or a confirmation draws.
///
/// One shape for both, because `ConfirmInjection` answers with the same
/// `ScanInjectionResponse` a scan does — confirming is a state change reported as
/// a fresh scan, not a different report.
fn injection_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("what", 16),
        ReportColumn::new("detail", 46),
        ReportColumn::new("where", 10),
    ]
}

/// The account a policy verb applies to.
///
/// `--account` when given, otherwise 0 — and 0 is the *global* budget here rather
/// than "no account", which is the opposite of what it means to the daemon verbs.
/// The proto is explicit about it, so this reads the flag rather than falling back
/// to the account on screen: a budget silently written against whichever mailbox
/// happened to be open is the kind of surprise a spending cap must not have.
///
/// # Errors
///
/// A message naming the offending value when `--account` is not a number.
fn scope(invocation: &Invocation, _target: &Target) -> Result<i64, String> {
    match flag(invocation, "account") {
        None => Ok(0),
        Some(text) => text.parse::<i64>().map_err(|_| {
            format!("--account {text:?}: an account id, or omit it for the global scope")
        }),
    }
}

/// The caps a `:ai budget set --…` line carried, as text.
///
/// Text rather than numbers, because the form holds text and the wire seam is the
/// one place a cap becomes an `f64` or an `i64` — parsing here and again there
/// would be two validators to keep in step. What *is* checked here is that each
/// one is a number at all, so a typo is refused where it was typed.
///
/// # Errors
///
/// A message naming the offending flag and value.
fn caps(invocation: &Invocation) -> Result<Vec<(String, String)>, String> {
    let mut caps = Vec::new();
    for (name, _, _) in CAPS {
        let Some(text) = flag(invocation, name) else {
            continue;
        };
        let looks_numeric = if name.ends_with("usd") {
            text.parse::<f64>().is_ok_and(|value| value >= 0.0)
        } else {
            text.parse::<i64>().is_ok_and(|value| value >= 0)
        };
        if !looks_numeric {
            return Err(format!("--{name} {text:?}: a number, at least zero"));
        }
        caps.push((name.to_owned(), text.to_owned()));
    }
    Ok(caps)
}

/// The account the `:ai budget status` report is about, for the scope column.
#[must_use]
pub fn scope_label(account_id: i64) -> String {
    if account_id == 0 {
        "global".to_owned()
    } else {
        format!("account {account_id}")
    }
}

/// Whether a report is being asked for the scope this target names.
#[must_use]
pub fn class_label(class: Class) -> &'static str {
    match class {
        Class::All => "all",
        Class::Bulk => "bulk",
    }
}
