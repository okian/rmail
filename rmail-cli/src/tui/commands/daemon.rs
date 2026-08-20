//! The daemon-observability verbs (task 94): the index, sync, the AI dispatch
//! loop, the finder index, and client auth.
//!
//! One arm per verb, and nothing else — see `tui::commands`' module docs on why
//! this is a table rather than a dispatcher.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{account, no_account, Answer, Pause, Reindex, Request, Target};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// Ask the same question of a verb the confirmation gate asks: what does it
/// answer with, and does it ask first.
///
/// `None` for a verb this build has no answer for — which is a real state, not
/// an oversight: the registry declares verbs for tasks 95 onward, and
/// `tui::model` reports "no report for it" rather than pretending.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    let bang = invocation.bang;
    Some(match verb.as_str() {
        // -- the index ------------------------------------------------------
        "index status" => Request::rows(
            Cmd::IndexStatus { generation },
            "index — coverage and queue",
            vec![
                ReportColumn::new("stage", 12),
                ReportColumn::new("state", 10),
                ReportColumn::new("coverage", 10),
                ReportColumn::new("pending", 9),
                ReportColumn::new("quarantined", 12),
            ],
        ),
        "index run" => Request::rows(
            Cmd::IndexReindex {
                generation,
                mode: Reindex::Drain,
                mailbox_id: None,
            },
            "index run — draining the queue",
            progress_columns(),
        ),
        "index reindex" => {
            let Some(mailbox_id) = target.mailbox_id else {
                return Some(Answer::Refused(
                    "reindex works on the open folder — open one first".to_owned(),
                ));
            };
            Request::rows(
                Cmd::IndexReindex {
                    generation,
                    mode: Reindex::Selection,
                    mailbox_id: Some(mailbox_id),
                },
                "index reindex — this folder",
                progress_columns(),
            )
        }
        "index rebuild" => {
            let mut answer = Request::rows(
                Cmd::IndexRebuild { generation },
                "index rebuild — from scratch",
                progress_columns(),
            );
            // The one verb here that asks when typed in full, and the reason is
            // not that it mutates — `:index gc` mutates and does not ask.
            // Rebuild drops every derived row and re-derives it, which on a
            // large mailbox is minutes of work and leaves search degraded while
            // it runs. That is the shape of thing worth one keystroke of
            // friction, and the acceptance names it specifically.
            if let Answer::Rows(request) = &mut answer {
                request.confirm = (!bang).then(|| {
                    "rebuild the whole index? every derived row is dropped and \
                     re-derived, and search is degraded until it finishes [y/N]"
                        .to_owned()
                });
            }
            answer
        }
        "index verify" => Request::rows(
            Cmd::IndexVerify { generation },
            "index verify — drift",
            vec![
                ReportColumn::new("check", 26),
                ReportColumn::new("rows adrift", 12),
            ],
        ),
        "index gc" => Request::rows(
            Cmd::IndexGc { generation },
            "index gc — reclaimed",
            vec![ReportColumn::new("what", 22), ReportColumn::new("rows", 10)],
        ),
        "index entities" => {
            // Refused here rather than sent: `ListEntities` rejects an empty
            // kind, so a bare `:index entities` would be a round trip whose only
            // outcome is an error. The positional is declared *optional* so the
            // verb stays typeable — see `command::explicit`'s `KIND` — which
            // puts the refusal on this side.
            let Some(kind) = invocation.positionals.first().cloned() else {
                return Some(Answer::Refused(
                    "name a kind to list — email, phone, amount…".to_owned(),
                ));
            };
            Request::rows(
                Cmd::IndexEntities {
                    generation,
                    kind: kind.clone(),
                },
                &format!("index entities — {kind}"),
                vec![
                    ReportColumn::new("kind", 12),
                    ReportColumn::new("value", 34),
                    ReportColumn::new("mentions", 9),
                    ReportColumn::new("messages", 9),
                ],
            )
        }
        "index start" => Request::fact(
            Cmd::IndexSetPaused {
                pause: Pause::Start,
            },
            "starting the indexer…",
        ),
        "index stop" => Request::fact(
            Cmd::IndexSetPaused { pause: Pause::Stop },
            "stopping the indexer…",
        ),

        // -- sync -----------------------------------------------------------
        "sync status" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::SyncStatusReport {
                    generation,
                    account_id,
                },
                "sync — every folder",
                vec![
                    ReportColumn::new("folder", 26),
                    ReportColumn::new("messages", 10),
                    ReportColumn::new("walked", 8),
                    ReportColumn::new("last sync", 18),
                ],
            )
        }
        "sync now" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::rows(
                Cmd::SyncNow {
                    generation,
                    account_id,
                },
                "sync now",
                vec![
                    ReportColumn::new("folder", 22),
                    ReportColumn::new("strategy", 12),
                    ReportColumn::new("new", 6),
                    ReportColumn::new("flags", 7),
                    ReportColumn::new("expunged", 9),
                ],
            )
        }
        "sync pause" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::fact(
                Cmd::SyncSetPaused {
                    account_id,
                    pause: Pause::Stop,
                },
                "pausing sync…",
            )
        }
        "sync resume" => {
            let Some(account_id) = account(target) else {
                return Some(no_account());
            };
            Request::fact(
                Cmd::SyncSetPaused {
                    account_id,
                    pause: Pause::Start,
                },
                "resuming sync…",
            )
        }

        // -- the AI pipeline ------------------------------------------------
        "ai status" => Request::rows(
            Cmd::AiUsage {
                generation,
                costs: false,
            },
            "ai — the dispatch loop",
            vec![
                ReportColumn::new("what", 18),
                ReportColumn::new("value", 42),
            ],
        ),
        // The same RPC as `ai status`, and deliberately its own verb rather
        // than a flag: `mail` spells both (`AiGetUsage.cli()` lists them), and a
        // TUI that made one of them `--costs` would be the surface where the
        // spelling diverged.
        "ai cost" => Request::rows(
            Cmd::AiUsage {
                generation,
                costs: true,
            },
            "ai — spend and caps",
            vec![
                ReportColumn::new("window", 14),
                ReportColumn::new("spent", 12),
                ReportColumn::new("cap", 12),
                ReportColumn::new("tokens", 16),
            ],
        ),
        "ai pause" => Request::fact(
            Cmd::AiSetPaused { pause: Pause::Stop },
            "pausing AI dispatch…",
        ),
        "ai resume" => Request::fact(
            Cmd::AiSetPaused {
                pause: Pause::Start,
            },
            "resuming AI dispatch…",
        ),
        "ai retry" => Request::fact(Cmd::AiRetry, "retrying quarantined AI jobs…"),
        "ai process" => {
            let Some(message_id) = target.message_id else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            Request::rows(
                Cmd::AiProcess {
                    generation,
                    message_id,
                },
                "ai process — this message",
                vec![
                    ReportColumn::new("what", 16),
                    ReportColumn::new("value", 44),
                ],
            )
        }

        // -- the finder index -----------------------------------------------
        "finder status" => Request::rows(
            Cmd::FinderStatus { generation },
            "finder index",
            vec![
                ReportColumn::new("what", 18),
                ReportColumn::new("value", 30),
            ],
        ),
        "finder rebuild" => Request::fact(Cmd::FinderRebuild, "rebuilding the finder index…"),

        // -- client auth (task 90) ------------------------------------------
        "auth status" => Request::rows(
            Cmd::AuthStatus { generation },
            "auth — access to rmail's own API",
            vec![
                ReportColumn::new("setting", 21),
                ReportColumn::new("state", 48),
            ],
        ),
        "auth clear" => Request::fact(Cmd::AuthClear, "clearing the password…"),

        _ => return None,
    })
}

/// The columns every streamed indexing progress Report draws.
///
/// One shape for `run`, `reindex` and `rebuild` because the RPCs answer with the
/// same `IndexProgress`: three verbs with three column layouts over one message
/// would be three chances to disagree about what `remaining` means.
fn progress_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("counter", 14),
        ReportColumn::new("jobs", 12),
    ]
}
