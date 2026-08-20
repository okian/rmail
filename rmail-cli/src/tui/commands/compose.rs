//! The compose, send and follow-up verbs (task 100).
//!
//! One arm per verb, and nothing else — see `tui::commands`' module docs on why
//! this is a table rather than a dispatcher.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;
use rmail_core::compose::reply::Tone;

use super::{
    account, field_value_columns, flag, followup_columns, id_positional, no_account, no_id, switch,
    Answer, Request, Target,
};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// The compose verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    let bang = invocation.bang;
    Some(match verb.as_str() {
        // -- drafts (task 100) -----------------------------------------------
        "draft list" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::DraftList {
                    generation,
                    account_id,
                },
                "drafts",
                vec![
                    ReportColumn::new("id", 7),
                    ReportColumn::new("to", 24),
                    ReportColumn::new("subject", 30),
                    ReportColumn::new("updated", 16),
                ],
            )
        }
        "draft show" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            Request::rows(
                Cmd::DraftShow {
                    generation,
                    draft_id,
                },
                &format!("draft {draft_id}"),
                field_value_columns(),
            )
        }
        "draft edit" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            let Some(body) = flag(invocation, "body") else {
                return Some(Answer::Refused(
                    "name the new body — --body=\"...\"".to_owned(),
                ));
            };
            Request::rows(
                Cmd::DraftEdit {
                    generation,
                    draft_id,
                    body: body.to_owned(),
                },
                &format!("draft {draft_id} — updated"),
                field_value_columns(),
            )
        }
        "draft delete" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            let mut answer = Request::fact(
                Cmd::DraftDelete { draft_id },
                &format!("deleting draft {draft_id}…"),
            );
            if let Answer::Fact(request) = &mut answer {
                request.confirm = (!bang)
                    .then(|| format!("delete draft {draft_id}? this cannot be undone [y/N]"));
            }
            answer
        }
        "draft render" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            Request::rows(
                Cmd::DraftRender {
                    generation,
                    draft_id,
                },
                &format!("draft {draft_id} — rendered"),
                field_value_columns(),
            )
        }
        "draft rewrite" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            let tone = match flag(invocation, "tone") {
                Some(raw) => match Tone::parse(raw) {
                    Some(tone) => Some(tone.as_str().to_owned()),
                    None => {
                        return Some(Answer::Refused(format!(
                            "{raw}: not a tone — try {}",
                            Tone::ALL.map(Tone::as_str).join("/")
                        )))
                    }
                },
                None => None,
            };
            let shorter = switch(invocation, "shorter");
            let longer = switch(invocation, "longer");
            if shorter && longer {
                return Some(Answer::Refused(
                    "--shorter and --longer: pick one".to_owned(),
                ));
            }
            let instruction = flag(invocation, "instruction")
                .unwrap_or_default()
                .to_owned();
            // Refused here as well as server-side, the same reason and the
            // same message `mail draft rewrite` uses: a round trip to be
            // told the command asked for nothing is one that did not need
            // making.
            if tone.is_none() && !shorter && !longer && instruction.trim().is_empty() {
                return Some(Answer::Refused(
                    "nothing to do: give --tone, --shorter/--longer, or --instruction".to_owned(),
                ));
            }
            Request::rows(
                Cmd::DraftRewrite {
                    generation,
                    draft_id,
                    tone,
                    shorter,
                    longer,
                    instruction,
                },
                &format!("draft {draft_id} — rewriting"),
                field_value_columns(),
            )
        }
        "draft revisions" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            Request::rows(
                Cmd::DraftRevisions {
                    generation,
                    draft_id,
                },
                &format!("draft {draft_id} — revisions"),
                vec![
                    ReportColumn::new("seq", 5),
                    ReportColumn::new("label", 22),
                    ReportColumn::new("subject", 28),
                    ReportColumn::new("model", 16),
                ],
            )
        }
        "draft revert" => {
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            let seq = match invocation.positionals.get(1) {
                None => 0,
                Some(raw) => match raw.parse::<i64>() {
                    Ok(seq) => seq,
                    // Distinct from "not given": a typo here should not
                    // silently mean "the original", which is what falling
                    // through to the same default as no argument would.
                    Err(_) => return Some(Answer::Refused(format!("{raw:?} is not a revision"))),
                },
            };
            Request::rows(
                Cmd::DraftRevert {
                    generation,
                    draft_id,
                    seq,
                },
                &format!("draft {draft_id} — reverted to {seq}"),
                field_value_columns(),
            )
        }

        // -- send and the outbox (task 100) -----------------------------------
        "send" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let Some(draft_id) = flag(invocation, "draft").and_then(|raw| raw.parse::<i64>().ok())
            else {
                return Some(Answer::Refused(
                    "name the draft to send — --draft=<id>".to_owned(),
                ));
            };
            let at = flag(invocation, "at").unwrap_or_default().to_owned();
            let undo = match flag(invocation, "undo") {
                None => None,
                Some(raw) => match raw.parse::<i64>() {
                    // The proto: "it can only lengthen". Zero or negative
                    // would reach `OutboxPolicy::window` as `requested`, and
                    // `Some(0).unwrap_or(default)` does not fall back to it —
                    // it sends immediately with no undo at all, silently.
                    Ok(secs) if secs > 0 => Some(secs),
                    _ => {
                        return Some(Answer::Refused(format!(
                            "--undo={raw}: not a positive number of seconds"
                        )))
                    }
                },
            };
            Request::fact(
                Cmd::ScheduleSend {
                    account_id,
                    draft_id,
                    at,
                    undo,
                },
                &format!("sending draft {draft_id}…"),
            )
        }
        "outbox retry" => {
            let Some(outbox_id) = id_positional(invocation) else {
                return Some(no_id("outbox entry", "outbox"));
            };
            Request::fact(
                Cmd::RetryFailed { outbox_id },
                &format!("retrying {outbox_id}…"),
            )
        }
        "outbox reschedule" => {
            let Some(outbox_id) = id_positional(invocation) else {
                return Some(no_id("outbox entry", "outbox"));
            };
            let Some(at) = flag(invocation, "at") else {
                return Some(Answer::Refused("name a new time — --at=...".to_owned()));
            };
            Request::fact(
                Cmd::RescheduleSend {
                    outbox_id,
                    at: at.to_owned(),
                },
                &format!("rescheduling {outbox_id}…"),
            )
        }
        "outbox edit" => {
            let Some(outbox_id) = id_positional(invocation) else {
                return Some(no_id("outbox entry", "outbox"));
            };
            let Some(body) = flag(invocation, "body") else {
                return Some(Answer::Refused(
                    "name the new body — --body=\"...\"".to_owned(),
                ));
            };
            Request::fact(
                Cmd::UpdateScheduledBody {
                    outbox_id,
                    body: body.to_owned(),
                },
                &format!("updating {outbox_id}…"),
            )
        }
        "outbox send-now" => {
            let Some(outbox_id) = id_positional(invocation) else {
                return Some(no_id("outbox entry", "outbox"));
            };
            let mut answer = Request::fact(
                Cmd::SendNow { outbox_id },
                &format!("sending {outbox_id} now…"),
            );
            if let Answer::Fact(request) = &mut answer {
                request.confirm = (!bang)
                    .then(|| format!("send {outbox_id} now, skipping the rest of its wait? [y/N]"));
            }
            answer
        }
        "outbox suggest" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::SuggestSendTime {
                    generation,
                    account_id,
                },
                "outbox — a time to send",
                field_value_columns(),
            )
        }

        // -- follow-ups and the pre-send guardian (task 100) -------------------
        "followup list" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::FollowupList {
                    generation,
                    account_id,
                },
                "follow-ups",
                followup_columns(),
            )
        }
        "followup new" => {
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            let remind_in = flag(invocation, "in").unwrap_or_default().to_owned();
            let note = flag(invocation, "note").unwrap_or_default().to_owned();
            Request::fact(
                Cmd::FollowupNew {
                    message_id,
                    remind_in,
                    note,
                },
                "creating a follow-up…",
            )
        }
        "followup dismiss" => {
            let Some(id) = id_positional(invocation) else {
                return Some(no_id("follow-up", "followup list"));
            };
            Request::fact(Cmd::FollowupDismiss { id }, &format!("dismissing {id}…"))
        }
        "waiting" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let overdue = switch(invocation, "overdue");
            Request::rows(
                Cmd::Waiting {
                    generation,
                    account_id,
                    overdue,
                },
                if overdue {
                    "waiting on — overdue"
                } else {
                    "waiting on a reply"
                },
                followup_columns(),
            )
        }
        "nudge" => {
            let Some(id) = id_positional(invocation) else {
                return Some(no_id("waiting-on entry", "waiting"));
            };
            Request::rows(
                Cmd::DraftNudge { generation, id },
                &format!("nudge — {id}"),
                field_value_columns(),
            )
        }
        "preflight" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            let Some(draft_id) = id_positional(invocation) else {
                return Some(no_id("draft", "draft list"));
            };
            Request::rows(
                Cmd::PreflightCheck {
                    generation,
                    account_id,
                    draft_id,
                },
                &format!("preflight — draft {draft_id}"),
                vec![
                    ReportColumn::new("severity", 9),
                    ReportColumn::new("kind", 24),
                    ReportColumn::new("detail", 40),
                    ReportColumn::new("source", 8),
                ],
            )
        }

        _ => return None,
    })
}
