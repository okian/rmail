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
use rmail_core::compose::reply::Tone;

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
            let shorter = has_flag(invocation, "shorter");
            let longer = has_flag(invocation, "longer");
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
            let overdue = has_flag(invocation, "overdue");
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

/// The one positional every id-taking verb below declares (task 100). `None`
/// covers both "nothing was typed" and "what was typed is not a number" — the
/// caller's own refusal names the listing that has the real ids, which is the
/// answer either way.
fn id_positional(invocation: &Invocation) -> Option<i64> {
    invocation.positionals.first()?.parse().ok()
}

/// What a verb needing an id says when [`id_positional`] found none.
fn no_id(noun: &str, listing: &str) -> Answer {
    Answer::Refused(format!("name a {noun} — see `:{listing}`"))
}

/// A declared flag's value, if the line carried one.
fn flag<'a>(invocation: &'a Invocation, name: &str) -> Option<&'a str> {
    invocation
        .flags
        .iter()
        .find(|flag| flag.name == name)
        .and_then(|flag| flag.value.as_deref())
}

/// Whether a declared switch flag was set.
fn has_flag(invocation: &Invocation, name: &str) -> bool {
    invocation.flags.iter().any(|flag| flag.name == name)
}

/// The columns a report showing one item's fields draws — `:draft show` and
/// everything shaped like it (task 100): one row per field, rather than the
/// one-row-per-item table a listing draws. Shared for the same reason
/// [`progress_columns`] is: several verbs answer with a single item of
/// different underlying types, and a field/value pair is the one layout that
/// fits all of them without a column list per verb to keep in sync.
fn field_value_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("field", 10),
        ReportColumn::new("value", 60),
    ]
}

/// The columns `:followup list` and `:waiting` both draw. One shape for both
/// because `ListFollowupsResponse` and `ListWaitingOnResponse` answer with the
/// same `Followup` rows — `:waiting` is a filtered view of the same list, not
/// a different report.
fn followup_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("id", 6),
        ReportColumn::new("message", 24),
        ReportColumn::new("remind at", 17),
        ReportColumn::new("state", 10),
        ReportColumn::new("note", 20),
    ]
}
