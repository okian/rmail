//! `mail send` / `mail undo` / `mail outbox` / `mail followup` — thin gRPC
//! clients over `SendSchedulerService` (task 61).
//!
//! # `mail send` with no `--at` is still a scheduled send
//!
//! prd.md's model is that *every* outgoing message goes into the outbox, and
//! an "immediate" send is one scheduled at `now + undo_window`. The CLI
//! reflects that rather than hiding it: a plain `mail send` prints the id and
//! the deadline, because "press [u] to undo" is only meaningful if you know
//! what to undo and for how long.
//!
//! # Time expressions are resolved server-side
//!
//! `--at` is handed over verbatim. The daemon owns
//! `send.default_timezone`, the IANA zone database, and the deterministic
//! grammar (`rmail_core::outbox::schedule`), and resolving here would mean a
//! second parser that can disagree with it — including about which side of a
//! DST boundary "tomorrow 9am" falls on. The resolved absolute instant comes
//! back on the entry and is what gets printed, so a user confirms an instant
//! rather than an expression.

use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::{
    CancelRequest, CreateFollowupRequest, Followup, FollowupState, IdRequest, ListFollowupsRequest,
    ListOutboxRequest, OutboxEntry, OutboxState, RescheduleRequest, ScheduleSendRequest,
    SendOrigin, SuggestSendTimeRequest, UpdateBodyRequest,
};
use tonic::transport::Channel;

/// `mail send …`
#[derive(Debug, Args)]
pub struct SendArgs {
    /// Account to send as.
    #[arg(long)]
    account: i64,
    /// Send an existing draft. Its recipients, subject, and body are the
    /// message; the flags below are ignored.
    #[arg(long)]
    draft: Option<i64>,
    /// Recipient address. Repeat for several.
    #[arg(long)]
    to: Vec<String>,
    /// Carbon-copy address. Repeat for several.
    #[arg(long)]
    cc: Vec<String>,
    /// Blind-carbon-copy address. It reaches the SMTP envelope and never the
    /// message. Repeat for several.
    #[arg(long)]
    bcc: Vec<String>,
    /// Subject line.
    #[arg(long)]
    subject: Option<String>,
    /// Message body, inline.
    #[arg(long)]
    body: Option<String>,
    /// Message body, read from a file (`-` for stdin).
    #[arg(long, conflicts_with = "body")]
    body_file: Option<String>,
    /// The parent's Message-ID, for a reply composed inline.
    #[arg(long)]
    in_reply_to: Option<String>,
    /// When to send: an RFC 3339 instant, a natural-language expression
    /// ("tomorrow 9am", "next monday 8:30am", "in 30m"), or `optimal` to let
    /// the daemon pick a time inside the configured guardrails. Omit for an
    /// immediate (undoable) send.
    #[arg(long)]
    at: Option<String>,
    /// IANA timezone bare wall-clock expressions are read in. Defaults to
    /// `send.default_timezone`.
    #[arg(long)]
    tz: Option<String>,
    /// Lengthen the undo window for this send, in seconds.
    #[arg(long)]
    undo_window: Option<i64>,
    /// Send even if the pre-send guardian refuses the message.
    ///
    /// The guardian blocks on things that are irreversible once delivered — a
    /// credential in the body, an unfilled template hole — so this is the
    /// documented way through after reading what it found, not a way to turn
    /// the check off. The daemon logs every use.
    #[arg(long)]
    force: bool,
}

/// `mail undo [<id>]`
#[derive(Debug, Args)]
pub struct UndoArgs {
    /// The outbox entry to cancel. Omit for the most recent one that still
    /// can be.
    id: Option<i64>,
    /// Restrict a bare undo to one account.
    #[arg(long)]
    account: Option<i64>,
}

/// `mail outbox [<action>]`
#[derive(Debug, Args)]
pub struct OutboxArgs {
    /// Show only entries in this state.
    #[arg(long, value_parser = parse_state)]
    state: Option<i32>,
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    #[command(subcommand)]
    action: Option<OutboxAction>,
}

/// `mail outbox <action>`
#[derive(Debug, Subcommand)]
pub enum OutboxAction {
    /// Print one entry in full.
    Show {
        /// Outbox entry id.
        id: i64,
    },
    /// Cancel a scheduled send.
    Cancel {
        /// Outbox entry id.
        id: i64,
    },
    /// Move a scheduled send to a different time.
    Reschedule {
        /// Outbox entry id.
        id: i64,
        /// The new time: an instant or an expression, as `mail send --at`.
        #[arg(long)]
        at: String,
        /// IANA timezone to read `--at` in.
        #[arg(long)]
        tz: Option<String>,
    },
    /// Replace a scheduled message's body. Only entries scheduled from a
    /// draft can be edited.
    Edit {
        /// Outbox entry id.
        id: i64,
        /// The new body.
        #[arg(short = 'm', long = "message", required = true)]
        message: String,
    },
    /// Return a failed send to the queue.
    Retry {
        /// Outbox entry id.
        id: i64,
    },
    /// Make a scheduled send due immediately.
    #[command(name = "send-now")]
    SendNow {
        /// Outbox entry id.
        id: i64,
    },
    /// Ask for a send time inside the configured guardrails. Proposes only —
    /// nothing is scheduled.
    Suggest {
        /// Account to suggest for.
        #[arg(long)]
        account: i64,
        /// IANA timezone to suggest in.
        #[arg(long)]
        tz: Option<String>,
    },
}

/// `mail followup <action>`
#[derive(Debug, Subcommand)]
pub enum FollowupAction {
    /// Arm a reminder on a message.
    Add {
        /// The RFC 5322 Message-ID to follow up. Angle brackets optional.
        message_id: String,
        /// Account the message belongs to.
        #[arg(long)]
        account: i64,
        /// How long to wait ("3d", "12h"), or a time expression. Defaults to
        /// `send.followup.default_delay`.
        #[arg(long = "in")]
        remind_in: Option<String>,
        /// What to be reminded about.
        #[arg(long)]
        note: Option<String>,
        /// Keep nudging even if a reply arrives.
        #[arg(long)]
        no_cancel_on_reply: bool,
    },
    /// List reminders.
    List {
        /// Restrict to one account.
        #[arg(long)]
        account: Option<i64>,
        /// Show only reminders in this state.
        #[arg(long, value_parser = parse_followup_state)]
        state: Option<i32>,
    },
    /// Dismiss a reminder.
    Dismiss {
        /// Follow-up id.
        id: i64,
    },
}

// ---------------------------------------------------------------------------
// mail send
// ---------------------------------------------------------------------------

/// Run `mail send`.
pub async fn send(socket: &Path, args: SendArgs) -> Result<()> {
    if args.draft.is_none() && args.to.is_empty() && args.cc.is_empty() && args.bcc.is_empty() {
        bail!("a send needs --draft, or at least one of --to/--cc/--bcc");
    }
    let body = match (&args.body, &args.body_file) {
        (Some(body), _) => Some(body.clone()),
        (None, Some(path)) => Some(read_body(path).await?),
        (None, None) => None,
    };

    // `optimal` is a question, not a time — the daemon's grammar refuses it by
    // name — so it is translated into the request flag prd.md gives it rather
    // than passed through as an expression.
    let (send_at_nl, optimal) = match args.at.as_deref().map(str::trim) {
        Some("optimal") => (None, Some(true)),
        Some(expression) if !expression.is_empty() => (Some(expression.to_owned()), None),
        _ => (None, None),
    };

    let mut client = client(socket).await?;
    let entry = client
        .schedule_send(ScheduleSendRequest {
            account_id: args.account,
            draft_id: args.draft,
            to: args.to,
            cc: args.cc,
            bcc: args.bcc,
            subject: args.subject,
            body,
            in_reply_to: args.in_reply_to,
            send_at: None,
            send_at_nl,
            optimal,
            tz: args.tz.unwrap_or_default(),
            undo_window_secs: args.undo_window,
            origin: SendOrigin::User as i32,
            skip_preflight: args.force,
            // Empty: this command issues the RPC exactly once and never
            // retries it, so there is nothing for the fence to protect
            // against. Minting keys is a client policy task 42 owns.
            idempotency_key: String::new(),
        })
        .await
        .context("ScheduleSend RPC failed")?
        .into_inner();

    print_entry(&entry);
    if let Some(deadline) = entry.undo_deadline {
        let seconds = deadline
            .saturating_sub(chrono::Utc::now().timestamp())
            .max(0);
        println!("  undo with `mail undo {}` within {seconds}s", entry.id);
    }
    Ok(())
}

/// Run `mail undo`.
pub async fn undo(socket: &Path, args: UndoArgs) -> Result<()> {
    let mut client = client(socket).await?;
    let entry = client
        .cancel_scheduled(CancelRequest {
            id: args.id,
            account_id: args.account,
        })
        .await
        .context("CancelScheduled RPC failed")?
        .into_inner();
    println!("canceled #{} — {}", entry.id, subject_of(&entry));
    Ok(())
}

// ---------------------------------------------------------------------------
// mail outbox
// ---------------------------------------------------------------------------

/// Run `mail outbox …`.
pub async fn outbox(socket: &Path, args: OutboxArgs) -> Result<()> {
    let mut client = client(socket).await?;
    match args.action {
        None => {
            let response = client
                .list_outbox(ListOutboxRequest {
                    account_id: args.account,
                    state: args.state.unwrap_or(OutboxState::Unspecified as i32),
                    page_size: 0,
                    // First page only; paging is task 42's surface.
                    page_token: String::new(),
                })
                .await
                .context("ListOutbox RPC failed")?
                .into_inner();
            if response.entries.is_empty() {
                println!("outbox is empty");
                return Ok(());
            }
            for entry in &response.entries {
                print_row(entry);
            }
        }
        Some(OutboxAction::Show { id }) => {
            let entry = one(&mut client, args.account, id).await?;
            print_entry(&entry);
            if let Some(error) = &entry.last_error {
                println!("  last error: {error}");
            }
        }
        Some(OutboxAction::Cancel { id }) => {
            let entry = client
                .cancel_scheduled(CancelRequest {
                    id: Some(id),
                    account_id: None,
                })
                .await
                .context("CancelScheduled RPC failed")?
                .into_inner();
            println!("canceled #{}", entry.id);
        }
        Some(OutboxAction::Reschedule { id, at, tz }) => {
            let entry = client
                .reschedule_send(RescheduleRequest {
                    id,
                    send_at: None,
                    send_at_nl: Some(at),
                    tz: tz.unwrap_or_default(),
                })
                .await
                .context("RescheduleSend RPC failed")?
                .into_inner();
            print_entry(&entry);
        }
        Some(OutboxAction::Edit { id, message }) => {
            let entry = client
                .update_scheduled_body(UpdateBodyRequest { id, body: message })
                .await
                .context("UpdateScheduledBody RPC failed")?
                .into_inner();
            print_entry(&entry);
        }
        Some(OutboxAction::Retry { id }) => {
            let entry = client
                .retry_failed(IdRequest { id })
                .await
                .context("RetryFailed RPC failed")?
                .into_inner();
            print_entry(&entry);
        }
        Some(OutboxAction::SendNow { id }) => {
            let entry = client
                .send_now(IdRequest { id })
                .await
                .context("SendNow RPC failed")?
                .into_inner();
            print_entry(&entry);
        }
        Some(OutboxAction::Suggest { account, tz }) => {
            let response = client
                .suggest_send_time(SuggestSendTimeRequest {
                    account_id: account,
                    tz: tz.unwrap_or_default(),
                    not_before: None,
                })
                .await
                .context("SuggestSendTime RPC failed")?
                .into_inner();
            println!("{}  ({})", response.display, response.rationale);
        }
    }
    Ok(())
}

/// Fetch one entry through `ListOutbox`.
///
/// There is no `GetOutboxEntry` RPC — an outbox is small and always listed —
/// so `show` filters a full page rather than the service growing a thirteenth
/// method for a case the twelfth already covers. The page is asked for at the
/// server's cap so the answer is "no such entry" rather than "not in the first
/// fifty" for anything but an outbox of five hundred queued messages.
async fn one(
    client: &mut SendSchedulerServiceClient<Channel>,
    account_id: Option<i64>,
    id: i64,
) -> Result<OutboxEntry> {
    let response = client
        .list_outbox(ListOutboxRequest {
            account_id,
            state: OutboxState::Unspecified as i32,
            page_size: 500,
            page_token: String::new(),
        })
        .await
        .context("ListOutbox RPC failed")?
        .into_inner();
    response
        .entries
        .into_iter()
        .find(|entry| entry.id == id)
        .with_context(|| format!("no outbox entry {id}"))
}

// ---------------------------------------------------------------------------
// mail followup
// ---------------------------------------------------------------------------

/// Run `mail followup …`.
pub async fn followup(socket: &Path, action: FollowupAction) -> Result<()> {
    let mut client = client(socket).await?;
    match action {
        FollowupAction::Add {
            message_id,
            account,
            remind_in,
            note,
            no_cancel_on_reply,
        } => {
            let followup = client
                .create_followup(CreateFollowupRequest {
                    account_id: account,
                    message_id,
                    thread_id: None,
                    remind_at: None,
                    remind_in,
                    tz: String::new(),
                    note,
                    cancel_on_reply: no_cancel_on_reply.then_some(false),
                })
                .await
                .context("CreateFollowup RPC failed")?
                .into_inner();
            print_followup(&followup);
        }
        FollowupAction::List { account, state } => {
            let response = client
                .list_followups(ListFollowupsRequest {
                    account_id: account,
                    state: state.unwrap_or(FollowupState::Unspecified as i32),
                    page_size: 0,
                    page_token: String::new(),
                })
                .await
                .context("ListFollowups RPC failed")?
                .into_inner();
            if response.followups.is_empty() {
                println!("no follow-ups");
                return Ok(());
            }
            for followup in &response.followups {
                print_followup(followup);
            }
        }
        FollowupAction::Dismiss { id } => {
            let followup = client
                .dismiss_followup(IdRequest { id })
                .await
                .context("DismissFollowup RPC failed")?
                .into_inner();
            println!("dismissed #{}", followup.id);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

async fn client(socket: &Path) -> Result<SendSchedulerServiceClient<Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(SendSchedulerServiceClient::new(channel))
}

/// Read a body from a file, or from stdin when the path is `-`.
async fn read_body(path: &str) -> Result<String> {
    if path == "-" {
        // Blocking stdin on the runtime would stall every other task in this
        // process; there are none in a CLI, but the rule is not conditional.
        return tokio::task::spawn_blocking(|| {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer).map(|_| buffer)
        })
        .await
        .context("reading the message body from stdin")?
        .context("reading the message body from stdin");
    }
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading the message body from {path}"))
}

fn parse_state(value: &str) -> Result<i32, String> {
    let state = match value.to_ascii_lowercase().as_str() {
        "scheduled" => OutboxState::Scheduled,
        "sending" => OutboxState::Sending,
        "sent" => OutboxState::Sent,
        "failed" => OutboxState::Failed,
        "canceled" | "cancelled" => OutboxState::Canceled,
        other => {
            return Err(format!(
                "unknown state {other:?} (scheduled, sending, sent, failed, canceled)"
            ))
        }
    };
    Ok(state as i32)
}

fn parse_followup_state(value: &str) -> Result<i32, String> {
    let state = match value.to_ascii_lowercase().as_str() {
        "armed" => FollowupState::Armed,
        "fired" => FollowupState::Fired,
        "dismissed" => FollowupState::Dismissed,
        other => return Err(format!("unknown state {other:?} (armed, fired, dismissed)")),
    };
    Ok(state as i32)
}

fn state_name(state: i32) -> &'static str {
    match OutboxState::try_from(state) {
        Ok(OutboxState::Scheduled) => "scheduled",
        Ok(OutboxState::Sending) => "sending",
        Ok(OutboxState::Sent) => "sent",
        Ok(OutboxState::Failed) => "failed",
        Ok(OutboxState::Canceled) => "canceled",
        // Spelled out rather than abbreviated: this is the one state a user
        // has to act on, and "uncertain" alone reads like a warning they can
        // ignore.
        Ok(OutboxState::Uncertain) => "uncertain (may have been delivered)",
        Ok(OutboxState::Unspecified) | Err(_) => "unknown",
    }
}

fn followup_state_name(state: i32) -> &'static str {
    match FollowupState::try_from(state) {
        Ok(FollowupState::Armed) => "armed",
        Ok(FollowupState::Fired) => "fired",
        Ok(FollowupState::Dismissed) => "dismissed",
        Ok(FollowupState::Unspecified) | Err(_) => "unknown",
    }
}

fn subject_of(entry: &OutboxEntry) -> &str {
    if entry.subject.trim().is_empty() {
        "(no subject)"
    } else {
        entry.subject.as_str()
    }
}

/// Render a unix instant in the entry's own zone, falling back to the raw
/// number if that zone is not one this build knows.
fn at(instant: i64, tz: &str) -> String {
    let Some(utc) = chrono::DateTime::from_timestamp(instant, 0) else {
        return instant.to_string();
    };
    match tz.parse::<chrono_tz::Tz>() {
        Ok(zone) => utc
            .with_timezone(&zone)
            .format("%Y-%m-%d %H:%M %Z")
            .to_string(),
        Err(_) => utc.format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

fn print_row(entry: &OutboxEntry) {
    let late = if entry.sent_late { " (sent late)" } else { "" };
    println!(
        "#{:<5} {:<10} {}  {}  -> {}{late}",
        entry.id,
        state_name(entry.state),
        at(entry.send_at, &entry.tz),
        subject_of(entry),
        entry.to.join(", ")
    );
}

fn print_entry(entry: &OutboxEntry) {
    print_row(entry);
    if !entry.cc.is_empty() {
        println!("  cc: {}", entry.cc.join(", "));
    }
    if !entry.bcc.is_empty() {
        println!("  bcc: {}", entry.bcc.join(", "));
    }
    if entry.attempts > 0 {
        println!("  attempts: {}/{}", entry.attempts, entry.max_retries);
    }
}

fn print_followup(followup: &Followup) {
    println!(
        "#{:<5} {:<10} {}  {}",
        followup.id,
        followup_state_name(followup.state),
        at(followup.remind_at, &followup.tz),
        followup.note.as_deref().unwrap_or("(no note)")
    );
    println!("  message-id: <{}>", followup.message_id);
}
