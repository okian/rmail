//! The things a user names and keeps: notes, saved searches, smart folders
//! (task 99).
//!
//! # A saved search and a smart folder are not the same thing
//!
//! A saved search is a query stored under a name: running it searches, now, and
//! returns hits. A smart folder is a *predicate* with membership — messages enter
//! and leave it, it can auto-tag what enters, and evaluating it is a mutation for
//! that reason. The manual's `saved-vs-smart` page is the long form; what matters
//! here is that they are two families of verb, not one with a flag.
//!
//! `SavedSearchService` has no CLI surface at all, so — like `RuleService` at
//! task 95 — these spellings are what a future `mail saved` will have to adopt.
//! The smart-folder half does have one (`mail folder …`) and follows it exactly.
//!
//! # `:folder new` and `:folder compile` are two RPCs behind two verbs
//!
//! `CreateSmartFolder` takes a predicate written in the query operators;
//! `CompileSmartFolder` takes a *sentence* and has a model compile it once into a
//! stored plan. `mail folder new` spells both — with `--predicate` for the first
//! — and the acceptance's list names only one, so the second is a verb the
//! acceptance does not name and is documented as such. Two verbs rather than one
//! flag because one of them spends money at a provider and the other does not,
//! and that is not a difference to hide behind a flag's presence.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::super::{first, flag, switch, Answer, Request, Target};
use super::{account, count, on_screen};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// The columns a note listing draws.
fn note_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("id", 6),
        ReportColumn::new("author", 8),
        ReportColumn::new("written", 17),
        ReportColumn::new("note", 45),
    ]
}

/// Everything after the `n`th positional, joined.
///
/// `:note edit 4 the new text` is one note and one body; splitting the body word
/// by word would be the silent truncation `joined`'s own docs call out, and
/// `joined` itself would swallow the id.
fn rest(invocation: &Invocation, from: usize) -> String {
    invocation
        .positionals
        .iter()
        .skip(from)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The note, saved-search and smart-folder verbs' answers.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "note add" => {
            let message_id = match message_or_thread(invocation, target) {
                Ok(target) => target,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let body = super::super::joined(invocation);
            if body.trim().is_empty() {
                return Some(Answer::Refused(
                    "write something — :note add chased this on Tuesday".to_owned(),
                ));
            }
            Request::fact(
                Cmd::NoteAdd {
                    message_id: message_id.0,
                    thread: message_id.1,
                    body,
                },
                "adding the note…",
            )
        }
        "note list" => {
            let message_id = match message_or_thread(invocation, target) {
                Ok(target) => target,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::NoteList {
                    generation,
                    message_id: message_id.0,
                    thread: message_id.1,
                },
                "notes",
                note_columns(),
            )
        }
        "note watch" => {
            let message_id = match message_or_thread(invocation, target) {
                Ok(target) => target,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::NoteWatch {
                    generation,
                    message_id: message_id.0,
                    thread: message_id.1,
                },
                "notes — live, as they change",
                note_columns(),
            )
        }
        "note edit" => {
            let Some(note_id) = first(invocation).and_then(|id| id.parse::<i64>().ok()) else {
                return Some(Answer::Refused(
                    "which note — :note list has the ids".to_owned(),
                ));
            };
            let body = rest(invocation, 1);
            if body.trim().is_empty() {
                return Some(Answer::Refused(
                    "and what it should say now — :note edit 4 chased again".to_owned(),
                ));
            }
            Request::fact(Cmd::NoteEdit { note_id, body }, "rewriting the note…")
        }
        "note rm" => {
            let Some(note_id) = first(invocation).and_then(|id| id.parse::<i64>().ok()) else {
                return Some(Answer::Refused(
                    "which note — :note list has the ids".to_owned(),
                ));
            };
            Request::fact(Cmd::NoteDelete { note_id }, "deleting the note…")
        }
        "saved list" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::SavedList {
                    generation,
                    account_id,
                },
                "saved searches — <enter> runs one",
                vec![
                    ReportColumn::new("name", 20),
                    ReportColumn::new("query", 40),
                    ReportColumn::new("last run", 17),
                ],
            )
        }
        "saved save" | "saved edit" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "name it — :saved save unpaid is:unread from:stripe".to_owned(),
                ));
            };
            let query = rest(invocation, 1);
            if query.trim().is_empty() {
                return Some(Answer::Refused("and the query it stands for".to_owned()));
            }
            Request::fact(
                Cmd::SavedSet {
                    account_id,
                    name: name.clone(),
                    query,
                    // `Create` refuses a name that exists and `Update` refuses one
                    // that does not, which is what makes these two verbs rather
                    // than one: an upsert would hide a typo'd name as a new entry.
                    update: verb == "saved edit",
                },
                if verb == "saved edit" {
                    "rewriting the saved search…"
                } else {
                    "saving it…"
                },
            )
        }
        "saved run" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which one — :saved list has the names".to_owned(),
                ));
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::SavedRun {
                    generation,
                    account_id,
                    name: name.clone(),
                    limit,
                    explain: switch(invocation, "explain"),
                },
                &format!("saved run {name} — <enter> opens a hit"),
                vec![
                    ReportColumn::new("score", 7),
                    ReportColumn::new("from", 22),
                    ReportColumn::new("subject", 34),
                    ReportColumn::new("when", 17),
                ],
            )
        }
        "saved rm" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which one — :saved list has the names".to_owned(),
                ));
            };
            Request::fact(
                Cmd::SavedDelete {
                    account_id,
                    name: name.clone(),
                },
                "forgetting it…",
            )
        }
        "folder list" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::FolderList {
                    generation,
                    account_id,
                },
                "smart folders — <enter> lists what is in one",
                vec![
                    ReportColumn::new("name", 18),
                    ReportColumn::new("predicate", 34),
                    ReportColumn::new("auto-tag", 12),
                    ReportColumn::new("evaluated", 17),
                ],
            )
        }
        "folder new" | "folder compile" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "name it — :folder new unpaid is:unread from:stripe".to_owned(),
                ));
            };
            let text = rest(invocation, 1);
            if text.trim().is_empty() {
                return Some(Answer::Refused(if verb == "folder compile" {
                    "and what it should hold, in words".to_owned()
                } else {
                    "and its predicate, in the query operators".to_owned()
                }));
            }
            Request::rows(
                Cmd::FolderCreate {
                    generation,
                    account_id,
                    name: name.clone(),
                    text,
                    // The difference the two verbs exist for: one takes a
                    // predicate and reaches no model, the other has a sentence
                    // compiled once and stored.
                    compile: verb == "folder compile",
                    auto_tag: flag(invocation, "auto-tag").map(str::to_owned),
                    notify: switch(invocation, "notify"),
                    refresh: switch(invocation, "refresh"),
                },
                &format!("{verb} {name}"),
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 58),
                ],
            )
        }
        "folder members" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which folder — :folder list has the names".to_owned(),
                ));
            };
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::FolderMembers {
                    generation,
                    account_id,
                    name: name.clone(),
                    limit,
                },
                &format!("folder members {name} — <enter> opens one"),
                vec![
                    ReportColumn::new("message", 9),
                    ReportColumn::new("from", 24),
                    ReportColumn::new("subject", 36),
                    ReportColumn::new("when", 17),
                ],
            )
        }
        "folder eval" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which folder — :folder list has the names".to_owned(),
                ));
            };
            Request::rows(
                Cmd::FolderEval {
                    generation,
                    account_id,
                    name: name.clone(),
                },
                &format!("folder eval {name}"),
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 58),
                ],
            )
        }
        "folder rm" => {
            let account_id = match account(target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which folder — :folder list has the names".to_owned(),
                ));
            };
            Request::fact(
                Cmd::FolderDelete {
                    account_id,
                    name: name.clone(),
                },
                "forgetting it…",
            )
        }
        _ => return None,
    })
}

/// What a note verb is about: `(id, whether that id is a thread)`.
///
/// `NoteTarget` is a oneof of message or thread, so this returns which. The
/// message on screen is the default and `--thread` says the thread instead —
/// there is no separate thread id to type, because the TUI's notion of "this
/// thread" is the thread of the message under the cursor and the daemon resolves
/// it from the message.
///
/// # Errors
///
/// The refusal to show when there is no message on screen.
fn message_or_thread(invocation: &Invocation, target: &Target) -> Result<(i64, bool), String> {
    // `on_screen`, not `content::message`: `:note add`'s positionals are the note
    // body, so reading the first of them as an id would refuse every note that
    // does not begin with a number.
    let message_id = on_screen(target)?;
    Ok((message_id, switch(invocation, "thread")))
}
