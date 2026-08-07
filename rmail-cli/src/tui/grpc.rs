//! The [`CmdExec`] that turns a [`Cmd`] into real gRPC work.
//!
//! Every method here follows the same shape: clone a client (a tonic
//! `Channel` is a cheap, shareable handle), spawn a task, send a [`Msg`] when
//! it finishes. Nothing is awaited inline — [`CmdExec::exec`] is not `async`
//! and could not await even if it wanted to, which is precisely how prd.md's
//! "the TUI never blocks on network or model calls" is enforced rather than
//! merely intended.
//!
//! # Cancellation
//!
//! Every spawned task races a [`CancellationToken`] that [`GrpcExec::shutdown`]
//! fires when the TUI exits. Without it, quitting while a mutation is in
//! flight would leave the task running until its RPC returned — and the
//! `WatchEvents` stream, which by design never returns, would run until the
//! process died. "Don't leak tasks" is not rhetorical for a long-lived
//! server stream.
//!
//! # Deadlines
//!
//! Unary calls are wrapped in [`RPC_TIMEOUT`]. The UI does not block on them,
//! so a hung call costs no interactivity — but it would leave `inflight`
//! stuck above zero forever, and the status bar would keep claiming work is
//! happening. A timeout turns that into a visible error the user can act on.
//! The `WatchEvents` stream is deliberately exempt: a stream that is quiet
//! because no mail has arrived is working correctly.
//!
//! # Why the event stream is coalesced
//!
//! `WatchEvents` resumes from a cursor, and this client has no way to ask for
//! "the head" — `since_seq: 0` replays everything still inside the daemon's
//! retention window before following the tail live. Turning each of those
//! into a list reload would mean a burst of redundant reloads at startup. The
//! stream task instead sets a dirty flag and emits at most one
//! [`Msg::Changed`] per [`COALESCE`] window, so a backlog replay costs a
//! handful of cheap local reads and live mail still lands within a blink.

#[cfg(test)]
mod tests;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    CopyRequest, DeleteRequest, EventKind, GetMessageRequest, ListAccountsRequest,
    ListMessagesRequest, MoveRequest, SetFlagsRequest, SyncStatusRequest, WatchEventsRequest,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::StreamExt;
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::transport::Channel;

use super::html::{self, CommandOpener};
use super::model::drive::CmdExec;
use super::model::{wire, Cmd, Effect, Msg};

/// Deadline for a unary RPC. Generous: these are local reads over a Unix
/// socket, and the ones that reach IMAP (move/copy/delete) are several
/// commands each, every one of them already capped by
/// `rmail_core::imap::IMAP_DEADLINE`.
const RPC_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the event-stream task batches changes before telling the model.
const COALESCE: Duration = Duration::from_millis(300);

/// How many rows one folder listing asks for. `MailService.List` caps the
/// page server-side; this is a client-side statement of what fits on screen
/// with room to scroll, not an attempt to fetch the folder.
const PAGE_SIZE: i32 = 500;

/// Runs the TUI's commands against a live `rmaild`.
pub struct GrpcExec {
    mail: MailServiceClient<Channel>,
    sync: SyncServiceClient<Channel>,
    accounts: AccountServiceClient<Channel>,
    compose: ComposeServiceClient<Channel>,
    opener: CommandOpener,
    cancel: CancellationToken,
    /// Cancels `cancel` when this struct is dropped.
    ///
    /// Structural, not by convention: without it, "every task stops when the
    /// TUI exits" would depend on one caller remembering to call
    /// [`GrpcExec::shutdown`], and the `WatchEvents` stream — which by design
    /// never returns on its own — would outlive the session on any path that
    /// forgot.
    _guard: DropGuard,
}

impl GrpcExec {
    /// Connect to rmaild's Unix socket and build the four clients over one
    /// channel.
    ///
    /// # Errors
    ///
    /// If the socket cannot be reached — which is the one failure worth
    /// reporting before the TUI takes the terminal over, since a TUI that
    /// cannot reach the daemon has nothing to draw.
    pub async fn connect(socket: &Path) -> anyhow::Result<Self> {
        let channel = rmail_core::connect_uds(socket).await?;
        Ok(Self::with_channel(channel))
    }

    /// Build the four clients over an already-established channel.
    #[must_use]
    pub fn with_channel(channel: Channel) -> Self {
        let cancel = CancellationToken::new();
        Self {
            mail: MailServiceClient::new(channel.clone()),
            sync: SyncServiceClient::new(channel.clone()),
            accounts: AccountServiceClient::new(channel.clone()),
            compose: ComposeServiceClient::new(channel),
            opener: CommandOpener::platform(),
            _guard: cancel.clone().drop_guard(),
            cancel,
        }
    }

    /// Cancel every task still running, including the event stream.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// Spawn `work`, forwarding its message unless the TUI has shut down.
    fn spawn<F>(&self, out: UnboundedSender<Msg>, work: F)
    where
        F: Future<Output = Msg> + Send + 'static,
    {
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                msg = work => { let _ = out.send(msg); }
            }
        });
    }
}

impl CmdExec for GrpcExec {
    fn exec(&self, cmd: Cmd, out: UnboundedSender<Msg>) {
        match cmd {
            Cmd::LoadAccounts => {
                let mut client = self.accounts.clone();
                self.spawn(out, async move {
                    Msg::Accounts(call(client.list(ListAccountsRequest {})).await.map(|r| {
                        r.into_inner()
                            .accounts
                            .into_iter()
                            .map(wire::account)
                            .collect()
                    }))
                });
            }
            Cmd::LoadFolders { account_id } => {
                let mut client = self.sync.clone();
                self.spawn(out, async move {
                    Msg::Folders(
                        call(client.status(SyncStatusRequest { account_id }))
                            .await
                            .map(|r| {
                                r.into_inner()
                                    .folders
                                    .into_iter()
                                    .map(wire::folder)
                                    .collect()
                            }),
                    )
                });
            }
            Cmd::LoadMessages { mailbox_id } => {
                let mut client = self.mail.clone();
                self.spawn(out, async move {
                    let result = load_messages(&mut client, mailbox_id).await;
                    Msg::Messages { mailbox_id, result }
                });
            }
            Cmd::Open { message_id } => {
                let mut client = self.mail.clone();
                self.spawn(out, async move {
                    Msg::Opened {
                        message_id,
                        result: call(client.get(GetMessageRequest { id: message_id }))
                            .await
                            .map(|r| wire::open_message(r.into_inner())),
                    }
                });
            }
            Cmd::Watch { account_id } => self.watch(account_id, out),
            Cmd::SetFlags {
                message_id,
                flags,
                label,
            } => {
                let mut client = self.mail.clone();
                let applied = flags.clone();
                self.spawn(out, async move {
                    let result = call(client.set_flags(SetFlagsRequest { message_id, flags }))
                        .await
                        .map(|_| Effect::Flags {
                            message_id,
                            flags: applied,
                        });
                    Msg::Done { label, result }
                });
            }
            Cmd::Move {
                message_id,
                dest_mailbox_id,
                label,
            } => {
                let mut client = self.mail.clone();
                self.spawn(out, async move {
                    let result = call(client.r#move(MoveRequest {
                        message_id,
                        dest_mailbox_id,
                    }))
                    .await
                    .map(|_| Effect::Removed(message_id));
                    Msg::Done { label, result }
                });
            }
            Cmd::Copy {
                message_id,
                dest_mailbox_id,
            } => {
                let mut client = self.mail.clone();
                self.spawn(out, async move {
                    // A copy leaves the source alone and the new message is
                    // discovered by the destination folder's next sync, so
                    // there is nothing to change locally.
                    let result = call(client.copy(CopyRequest {
                        message_id,
                        dest_mailbox_id,
                    }))
                    .await
                    .map(|_| Effect::None);
                    Msg::Done {
                        label: "copied".to_owned(),
                        result,
                    }
                });
            }
            Cmd::Delete { message_id } => {
                let mut client = self.mail.clone();
                self.spawn(out, async move {
                    let result = call(client.delete(DeleteRequest { message_id }))
                        .await
                        .map(|_| Effect::Removed(message_id));
                    Msg::Done {
                        label: "deleted".to_owned(),
                        result,
                    }
                });
            }
            Cmd::Draft {
                kind,
                account_id,
                from,
                to,
                message_id,
            } => {
                let mut mail = self.mail.clone();
                let mut compose = self.compose.clone();
                self.spawn(out, async move {
                    let result = async {
                        let original = call(mail.get(GetMessageRequest { id: message_id }))
                            .await?
                            .into_inner();
                        let request = wire::draft_request(kind, account_id, &from, &to, &original);
                        let draft = call(compose.create_draft(request)).await?.into_inner();
                        Ok(Effect::Drafted(draft.id))
                    }
                    .await;
                    let label = match &result {
                        Ok(Effect::Drafted(id)) => {
                            format!("draft {id} created for {to} — edit it with `mail draft`")
                        }
                        _ => "draft".to_owned(),
                    };
                    Msg::Done { label, result }
                });
            }
            Cmd::OpenHtml { message_id } => {
                let mut client = self.mail.clone();
                let opener = self.opener.clone();
                self.spawn(out, async move {
                    let result = open_html(&mut client, message_id, opener).await;
                    Msg::Done {
                        label: "opened in browser".to_owned(),
                        result,
                    }
                });
            }
        }
    }
}

impl GrpcExec {
    /// Follow the daemon's event log for as long as the TUI runs.
    fn watch(&self, account_id: i64, out: UnboundedSender<Msg>) {
        let mut client = self.mail.clone();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let request = WatchEventsRequest {
                account_id,
                since_seq: 0,
                kinds: vec![
                    EventKind::NewMail as i32,
                    EventKind::FlagChanged as i32,
                    EventKind::Moved as i32,
                    EventKind::Deleted as i32,
                ],
            };
            // Losing the stream costs live updates, not the session — the
            // list still loads and every action still works. But "not fatal"
            // is not "not worth saying": this crate installs no `tracing`
            // subscriber, so a swallowed error here would leave the user with
            // a TUI that has silently stopped noticing new mail and a status
            // line that reads perfectly normal. It goes to the status line.
            let response = match client.watch_events(request).await {
                Ok(response) => response,
                Err(status) => {
                    let _ = out.send(Msg::LiveUpdatesStopped(status.message().to_owned()));
                    return;
                }
            };
            let mut stream = response.into_inner();
            let mut ticker = tokio::time::interval(COALESCE);
            let mut dirty = false;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        if dirty {
                            dirty = false;
                            if out.send(Msg::Changed).is_err() {
                                return;
                            }
                        }
                    }
                    next = stream.next() => match next {
                        Some(Ok(_)) => dirty = true,
                        // A stream error is terminal for this subscription
                        // (a retention gap is the documented case). Ending
                        // the task is right; resubscribing from a fresh
                        // cursor is task 85's reconnect work, not this
                        // shell's. Either way the user is told the feed
                        // stopped rather than left to infer it.
                        Some(Err(status)) => {
                            let _ = out.send(Msg::LiveUpdatesStopped(
                                status.message().to_owned(),
                            ));
                            return;
                        }
                        None => {
                            let _ = out.send(Msg::LiveUpdatesStopped(
                                "the daemon closed the stream".to_owned(),
                            ));
                            return;
                        }
                    },
                }
            }
        });
    }
}

/// Apply the unary deadline and flatten `tonic::Status` into a display
/// string. The TUI has one line to say what went wrong in; the status code is
/// not useful to a human, the message is.
async fn call<T>(
    future: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<tonic::Response<T>, String> {
    match tokio::time::timeout(RPC_TIMEOUT, future).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(status)) => Err(status.message().to_owned()),
        Err(_) => Err(format!("timed out after {}s", RPC_TIMEOUT.as_secs())),
    }
}

async fn load_messages(
    client: &mut MailServiceClient<Channel>,
    mailbox_id: i64,
) -> Result<Vec<super::model::MessageRow>, String> {
    let response = call(client.list(ListMessagesRequest {
        mailbox_id,
        page_size: PAGE_SIZE,
    }))
    .await?;
    let mut stream = response.into_inner();
    let mut rows = Vec::new();
    while let Some(message) = stream.next().await {
        rows.push(wire::message_row(
            message.map_err(|status| status.message().to_owned())?,
        ));
    }
    Ok(rows)
}

async fn open_html(
    client: &mut MailServiceClient<Channel>,
    message_id: i64,
    opener: CommandOpener,
) -> Result<Effect, String> {
    let full = call(client.get(GetMessageRequest { id: message_id }))
        .await?
        .into_inner();
    let Some(body) = wire::html_body(&full).map(str::to_owned) else {
        return Err("this message has no HTML part".to_owned());
    };
    // Creating the file and spawning the browser are both blocking syscalls;
    // they belong on the blocking pool, not on a runtime thread that is also
    // driving the event loop.
    tokio::task::spawn_blocking(move || html::open_in_browser(message_id, &body, &opener))
        .await
        .map_err(|error| format!("browser task failed: {error}"))?
        .map(|_| Effect::None)
        .map_err(|error| format!("{error:#}"))
}
