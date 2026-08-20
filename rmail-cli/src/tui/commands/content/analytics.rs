//! What the mailbox says about itself, and how to get it out (task 99).
//!
//! # `:export` writes files, and that is not a client this module invents
//!
//! `ExportService.Export` streams framed bytes; turning them back into an mbox
//! file, a Maildir tree or a directory of `.eml` files is
//! `rmail_core::export::write::DestinationWriter`'s job — shared code that also
//! owns the check keeping a server-supplied entry name inside the directory the
//! user named. `mail export` uses it, and so does the wire seam here. A second
//! writer in the TUI would be a second place that check could be got wrong.
//!
//! `--to` is required. There is no sensible default output directory for an
//! interactive session, and a verb that wrote somewhere the user had not named
//! would be the worst possible default.
//!
//! # Every report here names its own window
//!
//! `--since` is a duration and `--until` is an instant; see `content`'s module
//! docs on why the conversion happens at the wire seam. Every response carries
//! the window it actually summarized, and the reports draw it — a figure whose
//! period a reader has to assume is a figure they cannot check.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::super::{first, flag, joined, switch, Answer, Request, Target};
use super::{account, count, since, until};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// How an export is framed on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One file, messages concatenated with `From ` separators.
    Mbox,
    /// A Maildir tree.
    Maildir,
    /// One `.eml` file per message.
    Eml,
    /// One JSON document per message, including what the AI passes produced when
    /// `--with-ai` is given.
    Json,
}

impl Format {
    /// The framing `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "mbox" => Some(Self::Mbox),
            "maildir" => Some(Self::Maildir),
            "eml" => Some(Self::Eml),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Every framing, for the refusal that names them.
pub const FORMATS: [&str; 4] = ["mbox", "maildir", "eml", "json"];

/// What `:stats response-time` groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// One row per correspondent.
    Contact,
    /// One row per mailbox.
    Mailbox,
}

impl GroupBy {
    /// The grouping `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "contact" => Some(Self::Contact),
            "mailbox" => Some(Self::Mailbox),
            _ => None,
        }
    }
}

/// How `:attach invoices` is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceFormat {
    /// One row per invoice.
    Rows,
    /// A CSV document, for a spreadsheet.
    Csv,
}

impl InvoiceFormat {
    /// The framing `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "rows" => Some(Self::Rows),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }
}

/// The analytics and export verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "export" => {
            let Some(to) = flag(invocation, "to") else {
                return Some(Answer::Refused(
                    "--to names a directory to write into".to_owned(),
                ));
            };
            let format = match flag(invocation, "format") {
                // mbox, like `mail export`: one file is what somebody wants when
                // they have not said otherwise.
                None => Format::Mbox,
                Some(text) => match Format::parse(text) {
                    Some(format) => format,
                    None => {
                        return Some(Answer::Refused(format!(
                            "--format {text:?}: one of {}",
                            FORMATS.join(", ")
                        )))
                    }
                },
            };
            let thread = match count(invocation, "thread") {
                Ok(thread) => thread,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let query = joined(invocation);
            // The proto's selection is a oneof, so a request carrying both would
            // have one silently dropped. Refused here, where the line that named
            // both is still on screen.
            if thread.is_some() && !query.is_empty() {
                return Some(Answer::Refused(
                    "a query or --thread, not both — they select different things".to_owned(),
                ));
            }
            if thread.is_none() && query.is_empty() {
                return Some(Answer::Refused(
                    "say what to export — a query, or --thread=<id>".to_owned(),
                ));
            }
            Request::rows(
                Cmd::Export {
                    generation,
                    query,
                    thread_id: thread,
                    format,
                    to: to.to_owned(),
                    with_ai: switch(invocation, "with-ai"),
                    limit,
                },
                &format!("export → {to}"),
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 58),
                ],
            )
        }
        "stats response-time" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let group_by = match flag(invocation, "group-by") {
                None => GroupBy::Contact,
                Some(text) => match GroupBy::parse(text) {
                    Some(group_by) => group_by,
                    None => {
                        return Some(Answer::Refused(format!(
                            "--group-by {text:?}: contact or mailbox"
                        )))
                    }
                },
            };
            let window = match (since(invocation), until(invocation)) {
                (Ok(since), Ok(until)) => (since, until),
                (Err(why), _) | (_, Err(why)) => return Some(Answer::Refused(why)),
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let min_samples = match count(invocation, "min-samples") {
                Ok(min_samples) => min_samples,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::ResponseTimes {
                    generation,
                    account_id,
                    group_by,
                    since_secs: window.0,
                    until: window.1,
                    limit,
                    min_samples,
                },
                "stats response-time — how fast, and who is waiting",
                vec![
                    ReportColumn::new("who", 26),
                    ReportColumn::new("you p50", 10),
                    ReportColumn::new("you p90", 10),
                    ReportColumn::new("them p50", 10),
                    ReportColumn::new("waiting", 9),
                    ReportColumn::new("note", 22),
                ],
            )
        }
        "stats ask" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let question = joined(invocation);
            if question.is_empty() {
                return Some(Answer::Refused(
                    "ask something — :stats ask who do I owe replies to".to_owned(),
                ));
            }
            Request::rows(
                Cmd::AskAnalytics {
                    generation,
                    account_id,
                    question: question.clone(),
                    // Off unless asked: a narrative is a second model call over
                    // rows the report already shows.
                    narrate: switch(invocation, "narrate"),
                },
                "stats ask — the query it ran, and what it returned",
                vec![
                    ReportColumn::new("", 4),
                    ReportColumn::new("a", 18),
                    ReportColumn::new("b", 18),
                    ReportColumn::new("c", 18),
                    ReportColumn::new("d", 18),
                ],
            )
        }
        "digest" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let window = match (since(invocation), until(invocation)) {
                (Ok(since), Ok(until)) => (since, until),
                (Err(why), _) | (_, Err(why)) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::Digest {
                    generation,
                    account_id,
                    since_secs: window.0,
                    until: window.1,
                    // A digest is cached per window, and re-generating one costs
                    // a model call — so the default answers from the cache and
                    // `--force` is the deliberate act.
                    force: switch(invocation, "force"),
                },
                "digest — <enter> opens the message a line cites",
                vec![
                    ReportColumn::new("section", 18),
                    ReportColumn::new("line", 58),
                ],
            )
        }
        "contact" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(address) = first(invocation) else {
                return Some(Answer::Refused(
                    "whose — :contact ada@example.com".to_owned(),
                ));
            };
            let window = match (since(invocation), until(invocation)) {
                (Ok(since), Ok(until)) => (since, until),
                (Err(why), _) | (_, Err(why)) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::ContactInsight {
                    generation,
                    account_id,
                    address: address.clone(),
                    since_secs: window.0,
                    until: window.1,
                    // The briefing is a model call. `--metrics-only` is the way
                    // to get the numbers without spending anything.
                    metrics_only: switch(invocation, "metrics-only"),
                },
                &format!("contact {address}"),
                vec![
                    ReportColumn::new("what", 20),
                    ReportColumn::new("value", 56),
                ],
            )
        }
        "subs" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let window = match (since(invocation), until(invocation)) {
                (Ok(since), Ok(until)) => (since, until),
                (Err(why), _) | (_, Err(why)) => return Some(Answer::Refused(why)),
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::Subscriptions {
                    generation,
                    account_id,
                    since_secs: window.0,
                    until: window.1,
                    limit,
                    // Only the senders worth unsubscribing from, rather than
                    // every bulk sender.
                    candidates_only: switch(invocation, "candidates-only"),
                    // Classifying the unknowns is a model call, so it is opt-in.
                    classify_unknown: switch(invocation, "classify"),
                },
                "subs — who sends you bulk mail, and whether you read it",
                vec![
                    ReportColumn::new("sender", 28),
                    ReportColumn::new("kind", 14),
                    ReportColumn::new("messages", 9),
                    ReportColumn::new("read", 7),
                    ReportColumn::new("unsubscribe", 20),
                ],
            )
        }
        _ => return None,
    })
}

/// How `:attach invoices` was asked to be framed.
///
/// Here rather than in `attach`, next to the other framing enums, because a
/// reader looking for "what formats does this build know" should find them in one
/// place.
///
/// # Errors
///
/// A message naming both framings.
pub(super) fn invoice_format(invocation: &Invocation) -> Result<InvoiceFormat, String> {
    match flag(invocation, "format") {
        None => Ok(InvoiceFormat::Rows),
        Some(text) => {
            InvoiceFormat::parse(text).ok_or_else(|| format!("--format {text:?}: rows or csv"))
        }
    }
}
