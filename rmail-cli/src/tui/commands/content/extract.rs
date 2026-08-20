//! What a message contains, and what the index knows (task 99).
//!
//! # Extraction is a mutation, and the sink is why
//!
//! `ExtractEvents` and `ExtractTasks` look like reads and are not: `--model`
//! spends at a provider, and every call *claims* the items it returns in the
//! delivery table, which is what makes the command and webhook sinks idempotent.
//! A read that consumed an idempotency claim would be a read that changed what
//! the next call returns. `parity` records them as mutating for that reason, and
//! nothing here softens it.
//!
//! # `:search eval` is the one verb in this client that reads a file
//!
//! `SearchService.Evaluate` takes its judgments *by value* — deliberately, so the
//! daemon needs no filesystem access to whatever directory a client happens to be
//! in — and a golden set only exists as a file. So there is no flow that avoids
//! reading one, unlike task 95's `:rule add`, where a file path was rejected
//! precisely because a better flow existed (draft, read the dry run, store).
//! The read happens on a blocking task at the wire seam, through
//! `rmail_core::eval::GoldenSet`, which is the same parse `mail search eval`
//! performs — so a malformed set is refused with a message about a path the user
//! can see rather than an `INVALID_ARGUMENT` about a request they did not write.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::super::{first, flag, joined, switch, Answer, Request, Target};
use super::{account, count, message, since};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// Where an extraction's items are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// Return the `.ics` document and deliver nothing.
    Ics,
    /// Pipe each item to the configured command.
    Command,
    /// POST each item to the configured webhook.
    Webhook,
}

impl Sink {
    /// The sink `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ics" => Some(Self::Ics),
            "command" => Some(Self::Command),
            "webhook" => Some(Self::Webhook),
            _ => None,
        }
    }
}

/// Every sink, for the refusal that names them.
pub const SINKS: [&str; 3] = ["ics", "command", "webhook"];

/// Which retrieval arm `:search eval` scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keyword only.
    Lexical,
    /// Embeddings only.
    Semantic,
    /// Both, fused — what `Search` itself does.
    Hybrid,
}

impl Mode {
    /// The mode `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "lexical" => Some(Self::Lexical),
            "semantic" => Some(Self::Semantic),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// Every mode, for the refusal that names them.
pub const MODES: [&str; 3] = ["lexical", "semantic", "hybrid"];

/// The extraction and index verbs' answers.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "extract events" | "extract tasks" => {
            let message_id = match message(invocation, target) {
                Ok(message_id) => message_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let sink = match flag(invocation, "sink") {
                // Absent is the `.ics` document and no delivery — the reading
                // that changes nothing outside this daemon.
                None => Sink::Ics,
                Some(text) => match Sink::parse(text) {
                    Some(sink) => sink,
                    None => {
                        return Some(Answer::Refused(format!(
                            "--sink {text:?}: one of {}",
                            SINKS.join(", ")
                        )))
                    }
                },
            };
            let tasks = verb == "extract tasks";
            Request::rows(
                Cmd::Extract {
                    generation,
                    message_id,
                    tasks,
                    // An `.ics` attachment is parsed without a model; free text
                    // is where one helps, and it costs money.
                    use_model: switch(invocation, "model"),
                    sink,
                },
                &format!("{verb} {message_id}"),
                if tasks {
                    vec![
                        ReportColumn::new("task", 34),
                        ReportColumn::new("due", 17),
                        ReportColumn::new("priority", 9),
                        ReportColumn::new("from", 14),
                    ]
                } else {
                    vec![
                        ReportColumn::new("event", 28),
                        ReportColumn::new("starts", 17),
                        ReportColumn::new("where", 18),
                        ReportColumn::new("from", 12),
                    ]
                },
            )
        }
        "extract data" => {
            let message_id = match message(invocation, target) {
                Ok(message_id) => message_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(schema) = flag(invocation, "schema") else {
                return Some(Answer::Refused(
                    "--schema names a configured extraction schema".to_owned(),
                ));
            };
            Request::rows(
                Cmd::ExtractData {
                    generation,
                    message_id,
                    schema: schema.to_owned(),
                    // An extraction is cached per (message, schema hash), and
                    // re-running one costs a model call.
                    refresh: switch(invocation, "refresh"),
                },
                &format!("extract data {message_id} — {schema}"),
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 58),
                ],
            )
        }
        "links" => {
            let message_id = match message(invocation, target) {
                Ok(message_id) => message_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::Links {
                    generation,
                    message_id,
                    use_model: switch(invocation, "model"),
                },
                &format!("links {message_id}"),
                vec![
                    ReportColumn::new("kind", 13),
                    ReportColumn::new("host", 24),
                    ReportColumn::new("text", 22),
                    ReportColumn::new("why", 24),
                ],
            )
        }
        "search compile" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let query = joined(invocation);
            if query.is_empty() {
                return Some(Answer::Refused(
                    "compile what — :search compile invoices from stripe last month".to_owned(),
                ));
            }
            Request::rows(
                Cmd::CompileQuery {
                    generation,
                    account_id,
                    query: query.clone(),
                    refresh: switch(invocation, "refresh"),
                },
                "search compile — the plan, before it runs",
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 58),
                ],
            )
        }
        "search entities" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let query = joined(invocation);
            if query.is_empty() {
                return Some(Answer::Refused(
                    "search for what — :search entities acme --kinds=org".to_owned(),
                ));
            }
            let window = match since(invocation) {
                Ok(since) => since,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::SearchEntities {
                    generation,
                    account_id,
                    query: query.clone(),
                    // Comma-separated or repeated, like every other list-shaped
                    // flag here. Not enumerated: the extractor's kinds grow, and
                    // a copy of the list in the client goes stale.
                    kinds: kinds(invocation),
                    since_secs: window,
                    limit,
                },
                &format!("search entities {query}"),
                vec![
                    ReportColumn::new("kind", 12),
                    ReportColumn::new("value", 30),
                    ReportColumn::new("mentions", 9),
                    ReportColumn::new("messages", 9),
                    ReportColumn::new("last seen", 17),
                ],
            )
        }
        "search eval" => {
            let Some(path) = first(invocation) else {
                return Some(Answer::Refused(
                    "which golden set — :search eval eval/golden.toml".to_owned(),
                ));
            };
            let mode = match flag(invocation, "mode") {
                None => None,
                Some(text) => match Mode::parse(text) {
                    Some(mode) => Some(mode),
                    None => {
                        return Some(Answer::Refused(format!(
                            "--mode {text:?}: one of {}",
                            MODES.join(", ")
                        )))
                    }
                },
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::SearchEval {
                    generation,
                    path: path.clone(),
                    mode,
                    limit,
                },
                &format!("search eval {path}"),
                vec![
                    ReportColumn::new("query", 26),
                    ReportColumn::new("ndcg@10", 9),
                    ReportColumn::new("mrr", 9),
                    ReportColumn::new("recall@50", 10),
                    ReportColumn::new("p@3", 9),
                    ReportColumn::new("note", 16),
                ],
            )
        }
        _ => return None,
    })
}

/// The entity kinds a line narrowed to.
///
/// Both spellings, comma-separated and repeated, for the reason `--scope` and
/// `--events` take both.
fn kinds(invocation: &Invocation) -> Vec<String> {
    invocation
        .flags
        .iter()
        .filter(|flag| flag.name == "kinds")
        .filter_map(|flag| flag.value.as_deref())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
        .collect()
}
