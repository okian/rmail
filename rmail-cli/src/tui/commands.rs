//! What each daemon verb answers with (task 94 onward).
//!
//! # One table, not a dispatcher
//!
//! Everything here is data: given a verb, the [`Cmd`] it issues, and — for a
//! verb that answers with rows — the title and columns its Report opens with.
//! Nothing in this module touches `Model`, opens an overlay or issues anything.
//! `tui::model`'s `run_daemon_command` is the single place that does, so the
//! twenty-odd verbs below share one implementation of the confirmation gate,
//! the generation stamp, the Report and the status line.
//!
//! That split is what makes the table testable. "`:index rebuild` refuses
//! without a bang", "`:ai cost` and `:ai status` are two views of one RPC",
//! "every verb the registry declares is answered here" — each is a claim about
//! this function and can be checked without a daemon, a terminal or a `Model`.
//!
//! # Rows or a fact
//!
//! A verb answering with more than one number opens a Report ([`Answer::Rows`]);
//! one answering with a single fact says so on the status line
//! ([`Answer::Fact`]). `:index gc` reclaimed seven different things and wants a
//! table; `:ai resume` either resumed or did not.
//!
//! The line is not "does it mutate". `:sync now` mutates and answers with a row
//! per folder, and reducing that to "synced" would throw away the one thing
//! somebody ran it to see.
//!
//! # Confirmation
//!
//! [`Request::confirm`] is per verb and deliberately not derived from
//! `parity::Command::effect`. Task 89 settled that a `:` line typed in full is
//! already the deliberate act a confirmation asks for, so gating every mutating
//! verb would make the question meaningless by asking it twenty times. What is
//! left is a judgement about *individual* verbs that are expensive and hard to
//! undo, which is a different question from "does this change state" — and it
//! rides on the spec rather than in a second table, so a verb cannot be added
//! without the author looking at the field.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::model::Cmd;
use super::report::ReportColumn;

/// Which pass `IndexService.Reindex` is being asked for.
///
/// The proto has four modes and the CLI spells each as its own verb; the TUI
/// takes the two that need no arguments beyond what is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reindex {
    /// `:index run` — drain whatever is already queued.
    Drain,
    /// `:index reindex` — re-enqueue the folder on screen.
    Selection,
}

/// Which way a pause verb goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// Stop.
    Stop,
    /// Start again.
    Start,
}

impl Pause {
    /// The `paused` flag this sends.
    #[must_use]
    pub const fn paused(self) -> bool {
        matches!(self, Self::Stop)
    }
}

/// What the model knows that a verb might need.
///
/// Passed in rather than read here, because this module has no `Model` — which
/// is the property that makes every claim below checkable without building one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The account on screen, or 0 when none has loaded.
    pub account_id: i64,
    /// The folder whose rows are listed, if one is open.
    pub mailbox_id: Option<i64>,
    /// The message the viewer or the list cursor is on, if any.
    pub message_id: Option<i64>,
}

/// What a verb answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Rows, in a Report.
    Rows(Box<Request>),
    /// A single fact, on the status line.
    Fact(Box<Request>),
    /// The verb needs something the screen does not have — no account loaded,
    /// no message under the cursor. Carries what to say about it.
    ///
    /// A refusal rather than a request with a zero in it: `AnalyzeMessage` on
    /// message 0 is `INVALID_ARGUMENT` from the daemon a round trip later, and
    /// "no message selected" said immediately is the same answer sooner.
    Refused(String),
}

/// One verb's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What to issue.
    pub cmd: Cmd,
    /// The Report's border, or — for a fact — what the status line says while
    /// the request is outstanding.
    pub title: String,
    /// The Report's columns. Empty for a fact.
    pub columns: Vec<ReportColumn>,
    /// What to ask before running it, when this verb is one that asks.
    ///
    /// `None` for almost everything; see the module docs on why this is a
    /// per-verb judgement and not `effect()`.
    pub confirm: Option<String>,
}

impl Request {
    /// A verb that answers with rows.
    fn rows(cmd: Cmd, title: &str, columns: Vec<ReportColumn>) -> Answer {
        Answer::Rows(Box::new(Self {
            cmd,
            title: title.to_owned(),
            columns,
            confirm: None,
        }))
    }

    /// A verb that answers with a fact.
    fn fact(cmd: Cmd, title: &str) -> Answer {
        Answer::Fact(Box::new(Self {
            cmd,
            title: title.to_owned(),
            columns: Vec::new(),
            confirm: None,
        }))
    }
}

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

/// What a verb needing an account says when none has loaded.
///
/// A named refusal rather than a `None` from [`answer`], because the two mean
/// different things to the caller: `None` is "this build has no answer for that
/// verb", and reporting a missing account as that would send somebody looking
/// for a feature that is present and simply has nothing to act on yet. Named
/// once rather than written at each of the four call sites, so they cannot
/// disagree about how to say it.
fn no_account() -> Answer {
    Answer::Refused("no account loaded yet".to_owned())
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

/// The account on screen, if one has loaded.
///
/// Zero means none: every account id in this API is a positive row id, and the
/// proto spells `0` as "every account" for the RPCs that accept it — which is a
/// different question from the one a bar zone or a folder listing is asking.
fn account(target: &Target) -> Option<i64> {
    (target.account_id != 0).then_some(target.account_id)
}
