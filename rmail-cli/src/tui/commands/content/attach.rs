//! What is attached, and reading inside it (task 99).
//!
//! # `:attach list` reaches nothing, on purpose
//!
//! The open message's parts are already in the model — `MailService.Get`
//! returned them, and the preview pane draws their names — so a listing verb
//! needs no RPC. It is answered in `tui::model`'s `run_invocation`, next to
//! `:account use`, for the reason that function's own comment gives: a round trip
//! to re-fetch what is on screen would be a second source of truth for one table.
//!
//! # Searching attachments has two paths to one capability
//!
//! `:attach search` and `:search attachments` are the same verb. That is
//! deliberate and precedented: `:helpgrep` and `:manual grep` are one action
//! under two paths for exactly this reason — the thing belongs to two families at
//! once, and making somebody remember which family a maintainer filed it under is
//! a worse cost than declaring both. `SearchService.SearchAttachments` is
//! searching (it reads the index) *and* it is about attachments.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::super::{flag, joined, switch, Answer, Request, Target};
use super::analytics::invoice_format;
use super::{account, count, message, on_screen, since, until};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// The columns an attachment hit draws.
fn hit_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("file", 24),
        ReportColumn::new("from", 20),
        ReportColumn::new("subject", 24),
        ReportColumn::new("where", 10),
        ReportColumn::new("excerpt", 30),
    ]
}

/// The attachment verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "attach tables" => {
            let message_id = match message(invocation, target) {
                Ok(message_id) => message_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AttachTables {
                    generation,
                    message_id,
                    // Absent means every part the extractor recognises; naming one
                    // is how a message with five spreadsheets is narrowed.
                    part: flag(invocation, "part").map(str::to_owned),
                    // A spreadsheet or a CSV is parsed without a model; HTML and
                    // scanned documents are where one helps, and it costs money,
                    // so it is opt-in.
                    allow_model: switch(invocation, "model"),
                },
                &format!("attach tables {message_id}"),
                vec![
                    ReportColumn::new("table", 18),
                    ReportColumn::new("a", 14),
                    ReportColumn::new("b", 14),
                    ReportColumn::new("c", 14),
                    ReportColumn::new("d", 14),
                ],
            )
        }
        "attach invoice" => {
            let message_id = match message(invocation, target) {
                Ok(message_id) => message_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AttachInvoice {
                    generation,
                    message_id,
                    part: flag(invocation, "part").map(str::to_owned),
                    use_model: switch(invocation, "model"),
                },
                &format!("attach invoice {message_id}"),
                vec![
                    ReportColumn::new("field", 16),
                    ReportColumn::new("value", 34),
                    ReportColumn::new("from", 24),
                ],
            )
        }
        "attach invoices" => {
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
            let format = match invoice_format(invocation) {
                Ok(format) => format,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AttachInvoices {
                    generation,
                    account_id,
                    vendor: flag(invocation, "vendor").map(str::to_owned),
                    since_secs: window.0,
                    until: window.1,
                    limit,
                    format,
                },
                "attach invoices — everything already extracted",
                vec![
                    ReportColumn::new("vendor", 22),
                    ReportColumn::new("number", 16),
                    ReportColumn::new("total", 14),
                    ReportColumn::new("issued", 12),
                    ReportColumn::new("due", 12),
                    ReportColumn::new("status", 10),
                ],
            )
        }
        "attach ask" => {
            let question = joined(invocation);
            if question.is_empty() {
                return Some(Answer::Refused(
                    "ask something — :attach ask what is the total on this invoice".to_owned(),
                ));
            }
            let all = switch(invocation, "all");
            // Scoped to the open message unless `--all`, which is the narrow
            // default on purpose: retrieval across every attachment in an account
            // is a much larger model call, and somebody looking at a document
            // usually means that document.
            // `on_screen`, not `message`: this verb's positionals are the
            // question, so reading the first of them as an id would refuse every
            // question that does not begin with a number.
            let message_id = if all {
                0
            } else {
                match on_screen(target) {
                    Ok(message_id) => message_id,
                    Err(why) => return Some(Answer::Refused(why)),
                }
            };
            let account_id = match (all, account(target)) {
                (false, _) => 0,
                (true, Ok(account_id)) => account_id,
                (true, Err(why)) => return Some(Answer::Refused(why)),
            };
            let top_k = match count(invocation, "top-k") {
                Ok(top_k) => top_k,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AttachAsk {
                    generation,
                    question: question.clone(),
                    message_id,
                    account_id,
                    part: flag(invocation, "part").map(str::to_owned),
                    top_k,
                },
                "attach ask — the answer, and what it read",
                vec![ReportColumn::new("", 4), ReportColumn::new("text", 72)],
            )
        }
        "attach search" | "search attachments" => {
            let query = joined(invocation);
            if query.is_empty() {
                return Some(Answer::Refused(
                    "search for what — :attach search invoice 2024".to_owned(),
                ));
            }
            let all = switch(invocation, "all");
            // Zero is "the whole account" on this RPC, and with no message on
            // screen that is the useful reading of an unscoped search rather than
            // a refusal — unlike `:attach ask`, where the question is usually
            // about the document somebody is looking at.
            let message_id = if all {
                0
            } else {
                on_screen(target).unwrap_or_default()
            };
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AttachSearch {
                    generation,
                    query: query.clone(),
                    account_id,
                    message_id,
                    limit,
                },
                &format!("{verb} {query}"),
                hit_columns(),
            )
        }
        _ => return None,
    })
}
