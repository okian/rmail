//! `mail notify watch` / `mail notify score` — the CLI surface for the
//! priority notification engine (`rmail_core::notify`,
//! `rmaild::NotificationApi`).
//!
//! # `watch` follows a cursor, not a socket
//!
//! `StreamAlerts` is resumable: every alert carries a monotonic id, and
//! `--since` replays everything after it before the live tail resumes. That
//! is what makes this command usable in a pipe — a script that records the
//! last id it printed can be restarted without losing the alert that fired
//! while it was down. Without `--since` the stream starts at the current head,
//! because "watch" means "from now on" and replaying a week of alerts into a
//! terminal on every invocation would be the wrong default. `--since 0`
//! replays the whole retained history, which is why the flag is passed through
//! as an `Option` rather than defaulted to a number here — see
//! `StreamAlertsRequest`'s own proto comment.
//!
//! # Thresholds are not settable from here
//!
//! They live in the operator's TOML (`[notify] threshold`, per-account
//! `[[accounts]] notify.threshold`) — there is no `mail notify set` because
//! there is no RPC behind it, deliberately (see `NotificationService`'s own
//! proto docs). `mail notify score` reports the *effective* threshold, which
//! is what someone debugging "why did this not ping me" actually needs.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use rmail_core::parity::Command;
use rmail_proto::v1::notification_service_client::NotificationServiceClient;
use rmail_proto::v1::{
    NotificationState, NotificationTier, ScoreMessageRequest, StreamAlertsRequest,
};

/// `mail notify <action>`.
#[derive(Debug, Subcommand)]
pub enum NotifyAction {
    /// Follow priority notifications as they fire
    /// (`NotificationService.StreamAlerts`).
    Watch {
        /// Resume strictly after this alert id, replaying the gap first.
        /// Omit to start at the current head; pass `0` to replay the whole
        /// retained history.
        #[arg(long)]
        since: Option<i64>,
        /// Stop after this many alerts (mainly for scripts and tests).
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Report what this daemon decided about one message, queueing a scoring
    /// pass if it has not decided yet
    /// (`NotificationService.ScoreMessage`).
    Score {
        /// The message id, as `mail search`/`mail list` report it.
        message_id: i64,
    },
}

/// Dispatch `mail notify <action>`.
///
/// # Errors
/// Any transport or RPC failure, surfaced with the context of what was being
/// attempted.
pub async fn run(socket: &Path, action: NotifyAction) -> Result<()> {
    match action {
        NotifyAction::Watch { since, limit } => watch(socket, since, limit).await,
        NotifyAction::Score { message_id } => score(socket, message_id).await,
    }
}

async fn client(socket: &Path) -> Result<NotificationServiceClient<crate::client::Client>> {
    let channel = crate::client::connect(socket).await?;
    Ok(NotificationServiceClient::new(channel))
}

async fn watch(socket: &Path, since: Option<i64>, limit: Option<u64>) -> Result<()> {
    let mut stream = client(socket)
        .await?
        // Passed straight through, `None` and all: an absent cursor is what
        // tells the daemon "from now on", and collapsing it to a number here
        // would take away the caller's ability to ask for history at all.
        .stream_alerts(StreamAlertsRequest { since_id: since })
        .await
        .context("StreamAlerts RPC failed")?
        .into_inner();

    // One sink for all three formats: in `table` mode `emit` answers `false`
    // and the row below is printed as it always was, so the loop body does not
    // fork three ways.
    let mut frames = crate::format::Frames::open(Command::NotificationStreamAlerts);
    // Held rather than propagated so `finish` runs even when the stream fails:
    // `--format json` writes its opening `[` with the first alert, and an
    // early return would leave an unterminated array on stdout.
    let outcome = async {
        let mut seen = 0u64;
        while let Some(alert) = stream
            .message()
            .await
            .context("the alert stream ended with an error")?
        {
            if !frames.emit(&alert)? {
                println!(
                    "{:>6}  {:<8} {:<16} {}",
                    alert.id,
                    tier_name(alert.tier),
                    // Attacker-controlled: an account label, a subject and a
                    // sender are all somebody else's text on their way to a
                    // terminal. The JSON path escapes them itself (see
                    // `crate::format`); this is the table path's half.
                    crate::terminal_safe(&alert.account),
                    crate::terminal_safe(&summary_line(
                        alert.subject.as_deref(),
                        alert.from.as_deref(),
                        &alert.reason
                    ))
                );
            }
            seen += 1;
            if limit.is_some_and(|limit| seen >= limit) {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    frames.finish()?;
    outcome
}

async fn score(socket: &Path, message_id: i64) -> Result<()> {
    let response = client(socket)
        .await?
        .score_message(ScoreMessageRequest { message_id })
        .await
        .context("ScoreMessage RPC failed")?
        .into_inner();

    if crate::format::emit_response(Command::NotificationScoreMessage, &response)? {
        return Ok(());
    }

    println!("state:      {}", state_name(response.state));
    match response.tier {
        Some(tier) => println!("tier:       {}", tier_name(tier)),
        None => println!("tier:       (not scored yet)"),
    }
    if let Some(reason) = &response.reason {
        println!("reason:     {reason}");
    }
    if !response.suppressed_reason.is_empty() {
        println!("suppressed: {}", response.suppressed_reason);
    }
    println!("threshold:  {}", response.effective_threshold);
    println!(
        "account:    {}",
        if response.account_enabled {
            "notifications enabled"
        } else {
            "notifications disabled"
        }
    );
    println!("would ping: {}", response.would_notify);
    Ok(())
}

/// `"Ada <ada@example.com>: Invoice overdue"`, falling back to the reason when
/// neither subject nor sender was included — `notify.include_subject` governs
/// what the *desktop* shows, but the alert stream is a local, authenticated
/// client of the daemon and carries what the daemon has.
fn summary_line(subject: Option<&str>, from: Option<&str>, reason: &str) -> String {
    match (from, subject) {
        (Some(from), Some(subject)) => format!("{from}: {subject} — {reason}"),
        (Some(from), None) => format!("{from} — {reason}"),
        (None, Some(subject)) => format!("{subject} — {reason}"),
        (None, None) => reason.to_owned(),
    }
}

fn tier_name(tier: i32) -> String {
    NotificationTier::try_from(tier)
        .map(|t| {
            t.as_str_name()
                .trim_start_matches("NOTIFICATION_TIER_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| format!("unknown({tier})"))
}

fn state_name(state: i32) -> String {
    NotificationState::try_from(state)
        .map(|s| {
            s.as_str_name()
                .trim_start_matches("NOTIFICATION_STATE_")
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|_| format!("unknown({state})"))
}

#[cfg(test)]
mod tests;
