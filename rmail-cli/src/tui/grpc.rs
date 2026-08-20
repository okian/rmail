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
use std::sync::Mutex;
use std::time::Duration;

use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::ai_policy_service_client::AiPolicyServiceClient;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::client_auth_service_client::ClientAuthServiceClient;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::finder_service_client::FinderServiceClient;
use rmail_proto::v1::index_service_client::IndexServiceClient;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    analyze_event, ask_chunk, draft_reply_event, AnalyzeMessageRequest, AskRequest,
    AuthStatusRequest, CancelRequest, ClearPasswordRequest, CopyRequest, CreateFollowupRequest,
    DeleteDraftRequest, DeleteRequest, DraftNudgeRequest, DraftReplyRequest, EventKind,
    ExplainRequest, FindRequest, FinderRebuildRequest, FinderStatusRequest, GetDraftRequest,
    GetMessageRequest, GetSpendRequest, GetSummaryRequest, GetUsageRequest, IdRequest,
    IndexGcRequest, IndexProgress, IndexStatusRequest, ListAccountsRequest,
    ListDraftRevisionsRequest, ListDraftsRequest, ListEntitiesRequest, ListFollowupsRequest,
    ListMessagesRequest, ListOutboxRequest, ListWaitingOnRequest, MoveRequest, PauseRequest,
    PreflightCheckRequest, RebuildRequest, ReindexMode, ReindexRequest, RenderDraftRequest,
    RescheduleRequest, ResumeRequest, RetryFailedRequest, RewriteDraftRequest, ScheduleSendRequest,
    SearchRequest, SelectDraftRevisionRequest, SetFlagsRequest, SetIndexPausedRequest,
    SetPausedRequest, SuggestReplyRequest, SuggestSendTimeRequest, SyncFolderRequest, SyncMode,
    SyncStatusRequest, UpdateBodyRequest, UpdateDraftRequest, VerifyIndexRequest,
    WatchEventsRequest,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::{CancellationToken, DropGuard};
/// The connection every client in this module is built on.
///
/// `crate::client::Client` rather than a bare `tonic` `Channel` so the TUI
/// honours the same global transport flags every other verb does — `--addr`,
/// `--token` and `--deadline` are attached by the interceptor that type
/// carries, and a TUI that quietly ignored them would be the one surface where
/// `--token` did nothing.
type Conn = crate::client::Client;

use super::commands;
use super::history;
use super::html::{self, CommandOpener};
use super::model::drive::CmdExec;
use super::model::{
    wire, write_keybinding, AskEvent, Cmd, Effect, FinderEvent, Msg, ReplyEvent, ReportEvent,
    SearchEvent, Stream,
};
use super::report::{ReportFill, ReportRow, ReportTone};
use super::status::{Health, Subsystem};

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

/// How long a keystroke waits before its search goes out.
///
/// The debounce lives here rather than in the model because the model is pure
/// and has no clock — `update` cannot sleep, and making it able to would make
/// blocking the UI expressible. Typing at speed therefore costs one round
/// trip, not one per character, and the character that stopped the typing is
/// the one that gets searched for.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

/// The finder's own. Shorter: its index is resident in memory and its whole
/// design point is per-keystroke latency, so waiting on it buys much less.
const FIND_DEBOUNCE: Duration = Duration::from_millis(40);

/// How many results the overlays ask for. A screenful with room to scroll —
/// the daemon caps both of these server-side anyway.
const OVERLAY_LIMIT: u32 = 50;

/// How many outbox entries one listing asks for.
const OUTBOX_PAGE: i32 = 200;

/// How often the undo countdown ticks.
const TICK: Duration = Duration::from_secs(1);

/// How many entities `:index entities` asks for.
///
/// A page, not the table: an extracted-entity table on a real mailbox has
/// hundreds of thousands of rows, and the Report caps at `report::MAX_ROWS`
/// anyway — asking for more than can be drawn is bytes over a socket nobody
/// will read.
const ENTITY_PAGE: i64 = 200;

/// How often the daemon heartbeat polls (task 92).
///
/// Five seconds is four local reads over a Unix socket — it costs the daemon
/// almost nothing and is well inside the time somebody would spend wondering
/// whether the indexer had stopped. Shorter would not make the answer more
/// useful; longer would mean a paused subsystem could sit unreported for as
/// long as somebody would reasonably keep looking at the bar.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Runs the TUI's commands against a live `rmaild`.
pub struct GrpcExec {
    mail: MailServiceClient<Conn>,
    sync: SyncServiceClient<Conn>,
    accounts: AccountServiceClient<Conn>,
    compose: ComposeServiceClient<Conn>,
    search: SearchServiceClient<Conn>,
    finder: FinderServiceClient<Conn>,
    ai: AiServiceClient<Conn>,
    scheduler: SendSchedulerServiceClient<Conn>,
    auth: ClientAuthServiceClient<Conn>,
    index: IndexServiceClient<Conn>,
    policy: AiPolicyServiceClient<Conn>,
    /// The task feeding the search overlay, so the next keystroke can abort
    /// it. One slot per stream kind: a search and a find can be outstanding
    /// at once (they are different overlays), but two searches cannot.
    ///
    /// Aborting is the client half of cancellation; the server half is
    /// supersession, which both services already do on their own. Neither is
    /// sufficient alone — the daemon cannot know the client stopped caring
    /// until the next request arrives, and the client cannot stop work the
    /// daemon has already started.
    searching: Mutex<Option<AbortHandle>>,
    finding: Mutex<Option<AbortHandle>>,
    asking: Mutex<Option<AbortHandle>>,
    /// `ComposeService.DraftReply`. A slot for the same reason `asking` is
    /// one: `Esc` needs one thing to abort, and a second `:reply --ai`
    /// before the first finishes is a supersession, not two replies racing
    /// to draft the same message.
    replying: Mutex<Option<AbortHandle>>,
    /// The why-panel's `Explain`. A slot even though the RPC is unary:
    /// holding `j` down the results issues one per row, each re-running the
    /// whole retrieval pipeline server-side, and only the last one can ever
    /// be drawn.
    explaining: Mutex<Option<AbortHandle>>,
    /// Whatever is feeding the Report overlay. A slot for the same reason
    /// `explaining` is one even though today's only reporting verb is unary:
    /// one report is on screen at a time, so a second request is always a
    /// supersession of the first, and `Esc` needs exactly one thing to abort
    /// whether the report was streaming or not.
    reporting: Mutex<Option<AbortHandle>>,
    /// The daemon heartbeat's loop. Superseding, so switching account restarts
    /// it rather than leaving two loops polling for two accounts — and so
    /// `shutdown` has one handle to stop.
    beating: Mutex<Option<AbortHandle>>,
    ticking: Mutex<Option<AbortHandle>>,
    /// The command-history write. Superseding, because `write_atomic`'s temp
    /// path is per-*process*: two commands in quick succession would
    /// otherwise have two tasks writing the same temp file and renaming it,
    /// and the whole list travels in each one anyway — so the newest is
    /// always the complete answer and the older is never worth finishing.
    saving: Mutex<Option<AbortHandle>>,
    opener: CommandOpener,
    /// The Unix socket this session connected over.
    ///
    /// Kept only for [`Cmd::AuthClear`], which — exactly as `mail auth clear`
    /// does — also forgets the session cached for this socket, since a cleared
    /// password makes it moot. `crate::session` is keyed by socket path, so
    /// the path has to travel with the executor; the model must not hold it,
    /// because `update` is pure and a path in it would invite a filesystem
    /// call from a place that cannot make one.
    socket: std::path::PathBuf,
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
    /// Connect to rmaild's Unix socket and build every client over one
    /// channel.
    ///
    /// # Errors
    ///
    /// If the socket cannot be reached — which is the one failure worth
    /// reporting before the TUI takes the terminal over, since a TUI that
    /// cannot reach the daemon has nothing to draw.
    pub async fn connect(socket: &Path) -> anyhow::Result<Self> {
        let channel = crate::client::connect(socket).await?;
        Ok(Self::with_channel(channel, socket))
    }

    /// Build every client over an already-established channel.
    ///
    /// `socket` is the path the channel was opened over — see
    /// [`GrpcExec::socket`] for the one command that needs it.
    #[must_use]
    pub fn with_channel(channel: Conn, socket: &Path) -> Self {
        let cancel = CancellationToken::new();
        Self {
            mail: MailServiceClient::new(channel.clone()),
            sync: SyncServiceClient::new(channel.clone()),
            accounts: AccountServiceClient::new(channel.clone()),
            compose: ComposeServiceClient::new(channel.clone()),
            search: SearchServiceClient::new(channel.clone()),
            finder: FinderServiceClient::new(channel.clone()),
            ai: AiServiceClient::new(channel.clone()),
            scheduler: SendSchedulerServiceClient::new(channel.clone()),
            auth: ClientAuthServiceClient::new(channel.clone()),
            index: IndexServiceClient::new(channel.clone()),
            policy: AiPolicyServiceClient::new(channel),
            searching: Mutex::new(None),
            finding: Mutex::new(None),
            asking: Mutex::new(None),
            replying: Mutex::new(None),
            explaining: Mutex::new(None),
            reporting: Mutex::new(None),
            beating: Mutex::new(None),
            ticking: Mutex::new(None),
            saving: Mutex::new(None),
            opener: CommandOpener::platform(),
            socket: socket.to_path_buf(),
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

    /// Spawn `work`, aborting whatever was previously in `slot`.
    ///
    /// This is what "keystroke cancellation" is on the client side: the task
    /// holding the old stream is dropped, which drops the `tonic` stream,
    /// which closes it. A stream `work` sends into after being aborted cannot
    /// exist — the abort happens between polls — so the model never has to
    /// reason about a half-delivered generation, only about a *stale* one.
    fn spawn_superseding<F>(&self, slot: &Mutex<Option<AbortHandle>>, work: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cancel = self.cancel.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = work => {}
            }
        });
        match slot.lock() {
            Ok(mut slot) => {
                if let Some(previous) = slot.replace(handle.abort_handle()) {
                    previous.abort();
                }
            }
            // Only reachable if a previous holder panicked while holding it,
            // which nothing here does. The new task still runs; the old one
            // ends when its own stream does. Degraded, never wedged.
            Err(poisoned) => tracing::warn!(
                error = %poisoned,
                "a superseding slot was poisoned; the previous stream was not aborted",
            ),
        }
        tracing::trace!("started a superseding stream");
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
                // One RPC, two messages. `SyncService.Status` is both the
                // folder listing and the sync indicator's own answer, and this
                // command is what a `WatchEvents` push triggers — so the
                // indicator is refreshed by the push rather than waiting out
                // the heartbeat's next tick, which is what the acceptance means
                // by "superseded by `WatchEvents` where those already push".
                // Sending the health first keeps that true even if the model
                // stops reading `Msg::Folders` for some reason.
                let reporter = out.clone();
                self.spawn(out, async move {
                    let response = call(client.status(SyncStatusRequest { account_id })).await;
                    let _ = reporter.send(Msg::Daemon {
                        subsystem: Subsystem::Sync,
                        result: response
                            .as_ref()
                            .map(|response| wire::sync_health(response.get_ref()))
                            .map_err(Clone::clone),
                    });
                    Msg::Folders(response.map(|r| {
                        r.into_inner()
                            .folders
                            .into_iter()
                            .map(wire::folder)
                            .collect()
                    }))
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
                    let result = call(client.set_flags(SetFlagsRequest {
                        message_id,
                        flags,
                        // Empty: a keystroke in the TUI is issued once and
                        // never auto-retried, so there is nothing for a replay
                        // fence to protect against. Minting keys is a client
                        // policy the CLI's gRPC layer owns (task 42).
                        idempotency_key: String::new(),
                    }))
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
                        idempotency_key: String::new(),
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
                        idempotency_key: String::new(),
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
                    let result = call(client.delete(DeleteRequest {
                        message_id,
                        idempotency_key: String::new(),
                    }))
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
            Cmd::Search {
                query,
                generation,
                account_id,
            } => {
                let mut client = self.search.clone();
                self.spawn_superseding(&self.searching, async move {
                    tokio::time::sleep(SEARCH_DEBOUNCE).await;
                    stream_search(&mut client, query, generation, account_id, &out).await;
                });
            }
            Cmd::Find {
                query,
                generation,
                account_id,
            } => {
                let mut client = self.finder.clone();
                self.spawn_superseding(&self.finding, async move {
                    tokio::time::sleep(FIND_DEBOUNCE).await;
                    stream_find(&mut client, query, generation, account_id, &out).await;
                });
            }
            Cmd::Explain {
                query,
                message_id,
                account_id,
            } => {
                let mut client = self.search.clone();
                self.spawn_superseding(&self.explaining, async move {
                    let result = call(client.explain(ExplainRequest {
                        query,
                        message_id,
                        account_id,
                        // The same question the hits were ranked under.
                        // `Search` collapses threads; explaining without it
                        // would re-derive a score for a page that was never
                        // the one on screen, and widens the window for the
                        // NOT_FOUND that a hit which no longer reproduces
                        // returns.
                        thread_collapse: true,
                        ..ExplainRequest::default()
                    }))
                    .await
                    .map(|response| wire::explanation(message_id, response.into_inner()));
                    let _ = out.send(Msg::Explained { message_id, result });
                });
            }
            Cmd::Ask {
                question,
                generation,
                account_id,
            } => {
                let mut client = self.ai.clone();
                self.spawn_superseding(&self.asking, async move {
                    stream_ask(&mut client, question, generation, account_id, &out).await;
                });
            }
            Cmd::LoadSummary {
                message_id,
                suggest_reply,
            } => {
                let mut client = self.ai.clone();
                self.spawn(out, async move {
                    let result = if suggest_reply {
                        call(client.suggest_reply(SuggestReplyRequest { message_id })).await
                    } else {
                        call(client.get_summary(GetSummaryRequest { message_id })).await
                    }
                    .map(|response| wire::summary(response.into_inner()));
                    Msg::Summarized { message_id, result }
                });
            }
            Cmd::LoadOutbox { account_id } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = list_outbox(&mut client, account_id).await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::CancelSend { outbox_id } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    // Cancel, then re-list: the fresh listing *is* the
                    // confirmation, and it is what takes the cancelled entry
                    // out of the pane and its toast off the screen. One
                    // command, one message, one `inflight` decrement.
                    let result = async {
                        let entry = call(client.cancel_scheduled(CancelRequest {
                            id: Some(outbox_id),
                            account_id: None,
                        }))
                        .await?
                        .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::Heartbeat { account_id } => {
                let mut sync = self.sync.clone();
                let mut index = self.index.clone();
                let mut ai = self.ai.clone();
                let mut policy = self.policy.clone();
                self.spawn_superseding(&self.beating, async move {
                    loop {
                        // Four independent messages rather than one combined
                        // answer, so a slow subsystem does not hold up the
                        // three that already replied — and so one failing
                        // leaves the other three's last-known state on the bar
                        // instead of blanking all four.
                        heartbeat(
                            &mut sync,
                            &mut index,
                            &mut ai,
                            &mut policy,
                            account_id,
                            &out,
                        )
                        .await;
                        // After, not before: the first round runs the moment
                        // the account is known, which is when somebody is
                        // most likely to be looking at the bar.
                        tokio::time::sleep(HEARTBEAT).await;
                    }
                });
            }
            Cmd::AuthStatus { generation } => {
                let mut client = self.auth.clone();
                let socket = self.socket.clone();
                // Through the superseding slot even though the RPC is unary.
                // Two reasons: `Esc` needs one thing to abort whichever kind of
                // report is running, and `r` supersedes by *issuing* rather than
                // by cancelling — which only works if the previous request is in
                // a slot the new one replaces.
                self.spawn_superseding(&self.reporting, async move {
                    auth_status(&mut client, &socket, generation, &out).await;
                });
            }
            Cmd::AuthClear => {
                let mut client = self.auth.clone();
                let socket = self.socket.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: "password cleared".to_owned(),
                        result: clear_password(&mut client, &socket).await,
                    }
                });
            }
            Cmd::IndexStatus { generation } => {
                let mut client = self.index.clone();
                self.report(generation, out, async move {
                    call(client.status(IndexStatusRequest {}))
                        .await
                        .map(|r| wire::index_status_rows(&r.into_inner()))
                });
            }
            Cmd::IndexReindex {
                generation,
                mode,
                mailbox_id,
            } => {
                let mut client = self.index.clone();
                self.stream_report(generation, out, move |sink| async move {
                    let request = ReindexRequest {
                        mode: match mode {
                            commands::Reindex::Drain => ReindexMode::Drain as i32,
                            commands::Reindex::Selection => ReindexMode::Selection as i32,
                        },
                        mailbox_id,
                        ..ReindexRequest::default()
                    };
                    drain_progress(client.reindex(request), sink).await
                });
            }
            Cmd::IndexRebuild { generation } => {
                let mut client = self.index.clone();
                self.stream_report(generation, out, move |sink| async move {
                    // `confirm: true` is honest here rather than a rubber stamp:
                    // the model asked before issuing this at all (or the caller
                    // typed a bang, which is what `mail index rebuild --yes`
                    // means), so by the time the request is built the question
                    // has an answer. The daemon's own `FAILED_PRECONDITION`
                    // guard stays the backstop for a client that skipped it.
                    drain_progress(
                        client.rebuild(RebuildRequest {
                            confirm: true,
                            ..RebuildRequest::default()
                        }),
                        sink,
                    )
                    .await
                });
            }
            Cmd::IndexVerify { generation } => {
                let mut client = self.index.clone();
                self.report(generation, out, async move {
                    call(client.verify(VerifyIndexRequest {}))
                        .await
                        .map(|r| wire::index_drift_rows(&r.into_inner()))
                });
            }
            Cmd::IndexGc { generation } => {
                let mut client = self.index.clone();
                self.report(generation, out, async move {
                    // False, which is `mail index gc`'s own default: the
                    // caches invalidate structurally, so a sweep is not needed
                    // in normal operation — and each discarded query plan is a
                    // paid model call that would be paid again. The CLI opts in
                    // with `--purge-caches`; a TUI verb spelled the same as a
                    // CLI one has to default the same way.
                    call(client.gc(IndexGcRequest {
                        purge_search_caches: false,
                    }))
                    .await
                    .map(|r| wire::index_gc_rows(&r.into_inner()))
                });
            }
            Cmd::IndexEntities { generation, kind } => {
                let mut client = self.index.clone();
                self.report(generation, out, async move {
                    // The kind is passed through unvalidated: the daemon's own
                    // refusal lists every kind it knows, and a second copy of
                    // that list here is a copy that goes stale the first time
                    // the extractor learns a new one.
                    call(client.list_entities(ListEntitiesRequest {
                        kind,
                        value: None,
                        limit: ENTITY_PAGE,
                    }))
                    .await
                    .map(|r| wire::index_entity_rows(&r.into_inner()))
                });
            }
            Cmd::IndexSetPaused { pause } => {
                let mut client = self.index.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: paused_label("indexer", pause),
                        result: call(client.set_paused(SetIndexPausedRequest {
                            paused: pause.paused(),
                        }))
                        .await
                        .map(|_| Effect::None),
                    }
                });
            }
            Cmd::SyncStatusReport {
                generation,
                account_id,
            } => {
                let mut client = self.sync.clone();
                self.report(generation, out, async move {
                    call(client.status(SyncStatusRequest { account_id }))
                        .await
                        .map(|r| wire::sync_status_rows(&r.into_inner()))
                });
            }
            Cmd::SyncNow {
                generation,
                account_id,
            } => {
                let mut client = self.sync.clone();
                self.report(generation, out, async move {
                    // `mailbox_id: None` is every folder, and `Auto` is the mode
                    // `mail sync` uses without `--full`: a TUI verb spelled the
                    // same as a CLI one has to mean the same thing.
                    call(client.sync_folder(SyncFolderRequest {
                        account_id,
                        mailbox_id: None,
                        mode: SyncMode::Auto as i32,
                    }))
                    .await
                    .map(|r| wire::sync_now_rows(&r.into_inner()))
                });
            }
            Cmd::SyncSetPaused { account_id, pause } => {
                let mut client = self.sync.clone();
                self.spawn(out, async move {
                    let result = match pause {
                        commands::Pause::Stop => call(client.pause(PauseRequest { account_id }))
                            .await
                            .map(|_| Effect::None),
                        commands::Pause::Start => call(client.resume(ResumeRequest { account_id }))
                            .await
                            .map(|_| Effect::None),
                    };
                    Msg::Done {
                        label: paused_label("sync", pause),
                        result,
                    }
                });
            }
            Cmd::AiUsage { generation, costs } => {
                let mut client = self.ai.clone();
                self.report(generation, out, async move {
                    call(client.get_usage(GetUsageRequest {})).await.map(|r| {
                        let stats = r.into_inner();
                        if costs {
                            wire::ai_cost_rows(&stats)
                        } else {
                            wire::ai_status_rows(&stats)
                        }
                    })
                });
            }
            Cmd::AiSetPaused { pause } => {
                let mut client = self.ai.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: paused_label("AI dispatch", pause),
                        result: call(client.set_paused(SetPausedRequest {
                            paused: pause.paused(),
                        }))
                        .await
                        .map(|_| Effect::None),
                    }
                });
            }
            Cmd::AiRetry => {
                let mut client = self.ai.clone();
                self.spawn(out, async move {
                    let result = call(client.retry_failed(RetryFailedRequest {})).await;
                    match result {
                        // The count is the whole answer, so it goes in the label
                        // rather than being dropped for a generic "done".
                        Ok(response) => Msg::Done {
                            label: format!("{} job(s) requeued", response.into_inner().revived),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "ai retry".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::AiProcess {
                generation,
                message_id,
            } => {
                let mut client = self.ai.clone();
                self.stream_report(generation, out, move |sink| async move {
                    analyze(
                        client.analyze_message(AnalyzeMessageRequest { message_id }),
                        sink,
                    )
                    .await
                });
            }
            Cmd::FinderStatus { generation } => {
                let mut client = self.finder.clone();
                self.report(generation, out, async move {
                    call(client.index_status(FinderStatusRequest {}))
                        .await
                        .map(|r| wire::finder_status_rows(&r.into_inner()))
                });
            }
            Cmd::FinderRebuild => {
                let mut client = self.finder.clone();
                self.spawn(out, async move {
                    let result = call(client.rebuild_index(FinderRebuildRequest {})).await;
                    match result {
                        Ok(response) => Msg::Done {
                            label: format!(
                                "finder index rebuilt — {} entries",
                                response.into_inner().entries
                            ),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "finder rebuild".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }

            // -- reply and drafts (task 100) ---------------------------------
            Cmd::DraftReply {
                generation,
                message_id,
                intent,
                reply_all,
            } => {
                let mut client = self.compose.clone();
                self.spawn_superseding(&self.replying, async move {
                    stream_draft_reply(
                        &mut client,
                        message_id,
                        intent,
                        reply_all,
                        generation,
                        &out,
                    )
                    .await;
                });
            }
            Cmd::DraftList {
                generation,
                account_id,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.list_drafts(ListDraftsRequest {
                        account_id,
                        page_size: 0,
                        page_token: String::new(),
                    }))
                    .await
                    .map(|r| wire::draft_list_rows(&r.into_inner()))
                });
            }
            Cmd::DraftShow {
                generation,
                draft_id,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.get_draft(GetDraftRequest { draft_id }))
                        .await
                        .map(|r| wire::draft_fields(&r.into_inner()))
                });
            }
            Cmd::DraftEdit {
                generation,
                draft_id,
                body,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.update_draft(UpdateDraftRequest {
                        draft_id,
                        from: None,
                        to: None,
                        cc: None,
                        bcc: None,
                        subject: None,
                        body_text: Some(body),
                        body_html: None,
                        attachments: None,
                    }))
                    .await
                    .map(|r| wire::draft_fields(&r.into_inner()))
                });
            }
            Cmd::DraftDelete { draft_id } => {
                let mut client = self.compose.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: format!("draft {draft_id} deleted"),
                        result: call(client.delete_draft(DeleteDraftRequest { draft_id }))
                            .await
                            .map(|_| Effect::None),
                    }
                });
            }
            Cmd::DraftRender {
                generation,
                draft_id,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.render_draft(RenderDraftRequest { draft_id }))
                        .await
                        .map(|r| wire::rendered_draft_fields(&r.into_inner()))
                });
            }
            Cmd::DraftRewrite {
                generation,
                draft_id,
                tone,
                shorter,
                longer,
                instruction,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.rewrite_draft(RewriteDraftRequest {
                        draft_id,
                        tone: wire::rewrite_tone(tone.as_deref()) as i32,
                        length: wire::rewrite_length(shorter, longer) as i32,
                        instruction,
                    }))
                    .await
                    .map(|r| wire::draft_revision_fields(&r.into_inner()))
                });
            }
            Cmd::DraftRevisions {
                generation,
                draft_id,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.list_draft_revisions(ListDraftRevisionsRequest { draft_id }))
                        .await
                        .map(|r| wire::draft_revision_rows(&r.into_inner()))
                });
            }
            Cmd::DraftRevert {
                generation,
                draft_id,
                seq,
            } => {
                let mut client = self.compose.clone();
                self.report(generation, out, async move {
                    call(client.select_draft_revision(SelectDraftRevisionRequest { draft_id, seq }))
                        .await
                        .map(|r| wire::draft_fields(&r.into_inner()))
                });
            }

            // -- send and the outbox (task 100) ------------------------------
            Cmd::ScheduleSend {
                account_id,
                draft_id,
                at,
                undo,
            } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let entry = call(client.schedule_send(ScheduleSendRequest {
                            account_id,
                            draft_id: Some(draft_id),
                            send_at_nl: (!at.is_empty()).then_some(at),
                            undo_window_secs: undo,
                            // No fence, matching `mail send`'s own choice
                            // (`outbox_cli.rs`) rather than diverging from
                            // it: neither client auto-retries this call, so
                            // a duplicate send needs a person to type `:send`
                            // twice, which is the same deliberate repetition
                            // an unfenced `r` (`Cmd::Draft`) already accepts.
                            ..ScheduleSendRequest::default()
                        }))
                        .await?
                        .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::RetryFailed { outbox_id } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let entry = call(client.retry_failed(IdRequest { id: outbox_id }))
                            .await?
                            .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::RescheduleSend { outbox_id, at } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let entry = call(client.reschedule_send(RescheduleRequest {
                            id: outbox_id,
                            send_at: None,
                            send_at_nl: Some(at),
                            tz: String::new(),
                        }))
                        .await?
                        .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::UpdateScheduledBody { outbox_id, body } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let entry = call(client.update_scheduled_body(UpdateBodyRequest {
                            id: outbox_id,
                            body,
                        }))
                        .await?
                        .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::SendNow { outbox_id } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let entry = call(client.send_now(IdRequest { id: outbox_id }))
                            .await?
                            .into_inner();
                        list_outbox(&mut client, entry.account_id).await
                    }
                    .await;
                    Msg::Outbox {
                        now: now_unix(),
                        result,
                    }
                });
            }
            Cmd::SuggestSendTime {
                generation,
                account_id,
            } => {
                let mut client = self.scheduler.clone();
                self.report(generation, out, async move {
                    call(client.suggest_send_time(SuggestSendTimeRequest {
                        account_id,
                        tz: String::new(),
                        not_before: None,
                    }))
                    .await
                    .map(|r| wire::suggest_send_time_fields(&r.into_inner()))
                });
            }

            // -- follow-ups and the pre-send guardian (task 100) -------------
            Cmd::FollowupList {
                generation,
                account_id,
            } => {
                let mut client = self.scheduler.clone();
                self.report(generation, out, async move {
                    call(client.list_followups(ListFollowupsRequest {
                        account_id: Some(account_id),
                        ..ListFollowupsRequest::default()
                    }))
                    .await
                    .map(|r| wire::followup_rows(&r.into_inner().followups))
                });
            }
            Cmd::FollowupNew {
                message_id,
                remind_in,
                note,
            } => {
                let mut mail = self.mail.clone();
                let mut scheduler = self.scheduler.clone();
                self.spawn(out, async move {
                    let result = async {
                        let original = call(mail.get(GetMessageRequest { id: message_id }))
                            .await?
                            .into_inner();
                        let message = original.message.unwrap_or_default();
                        let Some(header) = message.message_id.filter(|id| !id.is_empty()) else {
                            return Err("that message has no Message-ID to follow up on".to_owned());
                        };
                        call(scheduler.create_followup(CreateFollowupRequest {
                            account_id: message.account_id,
                            message_id: header,
                            thread_id: message.thread_id,
                            remind_at: None,
                            remind_in: (!remind_in.is_empty()).then_some(remind_in),
                            tz: String::new(),
                            note: (!note.is_empty()).then_some(note),
                            cancel_on_reply: None,
                        }))
                        .await
                        .map(|r| r.into_inner())
                    }
                    .await;
                    let label = match &result {
                        Ok(followup) => {
                            format!(
                                "follow-up created — reminds {}",
                                wire::when(followup.remind_at)
                            )
                        }
                        Err(_) => "follow-up".to_owned(),
                    };
                    Msg::Done {
                        label,
                        result: result.map(|_| Effect::None),
                    }
                });
            }
            Cmd::FollowupDismiss { id } => {
                let mut client = self.scheduler.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: format!("follow-up {id} dismissed"),
                        result: call(client.dismiss_followup(IdRequest { id }))
                            .await
                            .map(|_| Effect::None),
                    }
                });
            }
            Cmd::Waiting {
                generation,
                account_id,
                overdue,
            } => {
                let mut client = self.scheduler.clone();
                self.report(generation, out, async move {
                    call(client.list_waiting_on(ListWaitingOnRequest {
                        account_id: Some(account_id),
                        overdue_only: overdue,
                        page_size: 0,
                        page_token: String::new(),
                    }))
                    .await
                    .map(|r| wire::followup_rows(&r.into_inner().followups))
                });
            }
            Cmd::DraftNudge { generation, id } => {
                let mut client = self.scheduler.clone();
                self.report(generation, out, async move {
                    call(client.draft_nudge(DraftNudgeRequest { id }))
                        .await
                        .map(|r| wire::draft_nudge_fields(&r.into_inner()))
                });
            }
            Cmd::PreflightCheck {
                generation,
                account_id,
                draft_id,
            } => {
                let mut client = self.scheduler.clone();
                self.report(generation, out, async move {
                    call(client.preflight_check(PreflightCheckRequest {
                        account_id,
                        draft_id: Some(draft_id),
                        ..PreflightCheckRequest::default()
                    }))
                    .await
                    .map(|r| wire::preflight_rows(&r.into_inner()))
                });
            }
            Cmd::CancelStream { which } => {
                let slot = match which {
                    Stream::Search => &self.searching,
                    Stream::Find => &self.finding,
                    Stream::Ask => &self.asking,
                    Stream::Reply => &self.replying,
                    Stream::Explain => &self.explaining,
                    Stream::Report => &self.reporting,
                };
                abort(slot);
            }
            Cmd::SaveHistory { entries } => {
                // `spawn_blocking`, because this is the one command whose
                // work is a filesystem write rather than an RPC, and a
                // synchronous write on a runtime thread is exactly what
                // CLAUDE.md's "never block the runtime" is about. Failure is
                // logged and dropped: a history file that cannot be written
                // must not take the command line down with it, and the next
                // recorded line writes the whole list again anyway.
                let path = history::path_from_env();
                self.spawn_superseding(&self.saving, async move {
                    let written = tokio::task::spawn_blocking(move || {
                        let result = history::write(&path, &entries);
                        (path, result)
                    })
                    .await;
                    if let Ok((path, Err(error))) = written {
                        tracing::warn!(
                            error = %error,
                            path = %path.display(),
                            "could not write the command history",
                        );
                    }
                });
            }
            Cmd::WriteKeybinding {
                path,
                mode,
                chord,
                action,
                label,
            } => {
                // Plain `spawn`, not `spawn_superseding`: two `:keys set`
                // invocations are independent edits, not the same growing
                // list `SaveHistory` re-sends whole — cancelling the first
                // because a second one landed would strand its `inflight`
                // increment with no `Msg::KeysWritten` ever arriving to
                // release it.
                self.spawn(out, async move {
                    let outcome = tokio::task::spawn_blocking(move || {
                        write_keybinding(&path, mode, &chord, action)
                    })
                    .await;
                    let result = match outcome {
                        Ok(result) => result,
                        Err(join_error) => Err(join_error.to_string()),
                    };
                    Msg::KeysWritten { label, result }
                });
            }
            Cmd::Countdown { until } => {
                self.spawn_superseding(&self.ticking, async move {
                    loop {
                        let now = now_unix();
                        // Sent before the deadline check so the frame that
                        // retires the toast is delivered rather than skipped.
                        if out.send(Msg::Tick(now)).is_err() || now >= until {
                            return;
                        }
                        tokio::time::sleep(TICK).await;
                    }
                });
            }
        }
    }
}

/// Stop whatever is in `slot`.
///
/// A poisoned lock leaves the task running rather than propagating: the
/// session stays usable, and the stream ends on its own when the daemon
/// finishes it.
fn abort(slot: &Mutex<Option<AbortHandle>>) {
    match slot.lock() {
        Ok(mut slot) => {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
        Err(poisoned) => tracing::warn!(
            error = %poisoned,
            "a superseding slot was poisoned; a stream was left running",
        ),
    }
}

/// The wall clock, as the undo countdown reads it.
///
/// Unix seconds, matching `outbox.undo_deadline`'s own units, and read here
/// rather than in the model because the model is pure by type.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Drive one `SearchService.Search` stream into the model.
///
/// Hits are forwarded one at a time, as they arrive: the whole point of that
/// RPC is that the first hit is flushed before the rest of the page is
/// computed, and collecting into a `Vec` would throw away exactly the latency
/// it exists to deliver.
async fn stream_search(
    client: &mut SearchServiceClient<Conn>,
    query: String,
    generation: u64,
    account_id: i64,
    out: &UnboundedSender<Msg>,
) {
    tracing::debug!(generation, account_id, "search stream opening");
    let request = SearchRequest {
        query,
        account_id,
        limit: OVERLAY_LIMIT,
        // One row per conversation, which is what a list of results should
        // be; the why-panel asks for the explanation separately, so `explain`
        // stays off and nobody pays for a breakdown they did not open.
        thread_collapse: true,
        ..SearchRequest::default()
    };
    let mut stream = match client.search(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => {
            let _ = out.send(Msg::Search {
                generation,
                event: SearchEvent::Done(Err(status.message().to_owned())),
            });
            return;
        }
    };
    loop {
        let event = match stream.next().await {
            Some(Ok(hit)) => SearchEvent::Hit(Box::new(wire::hit(hit))),
            Some(Err(status)) => SearchEvent::Done(Err(status.message().to_owned())),
            None => SearchEvent::Done(Ok(())),
        };
        let done = matches!(event, SearchEvent::Done(_));
        if out.send(Msg::Search { generation, event }).is_err() || done {
            return;
        }
    }
}

/// Drive one `FinderService.Find` stream into the model.
///
/// Each batch is a complete snapshot of the current top-K, so it is forwarded
/// whole and the model replaces what it was showing. A superseded stream ends
/// with `complete` set and `superseded` flagged rather than with a
/// `CANCELLED` status, which is why nothing here treats the end of a stream
/// as an error.
async fn stream_find(
    client: &mut FinderServiceClient<Conn>,
    query: String,
    generation: u64,
    account_id: i64,
    out: &UnboundedSender<Msg>,
) {
    tracing::debug!(generation, account_id, "find stream opening");
    let request = FindRequest {
        query,
        account_id,
        limit: OVERLAY_LIMIT,
        // The overlay highlights what matched, which is the one thing that
        // makes a fuzzy result legible.
        with_positions: true,
        ..FindRequest::default()
    };
    let mut stream = match client.find(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => {
            let _ = out.send(Msg::Finder {
                generation,
                event: FinderEvent::Failed(status.message().to_owned()),
            });
            return;
        }
    };
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(batch) => FinderEvent::Batch {
                items: batch.results.into_iter().map(wire::finder_item).collect(),
                complete: batch.complete,
                superseded: batch.superseded,
                scanned: batch.scanned,
            },
            Err(status) => FinderEvent::Failed(status.message().to_owned()),
        };
        let failed = matches!(event, FinderEvent::Failed(_));
        if out.send(Msg::Finder { generation, event }).is_err() || failed {
            return;
        }
    }
}

/// Drive one `AiService.AskMailbox` stream into the model.
///
/// The frame order is the proto's and is not re-derived here: trace, then
/// tokens, then citations, then usage, then done. A stream that ends without
/// a `done` frame ended abnormally — that RPC terminates with `CANCELLED`
/// rather than a clean `OK` precisely so a partial answer cannot read as a
/// whole one — so this reports that rather than letting the pane sit on
/// "streaming" forever.
async fn stream_ask(
    client: &mut AiServiceClient<Conn>,
    question: String,
    generation: u64,
    account_id: i64,
    out: &UnboundedSender<Msg>,
) {
    tracing::debug!(generation, account_id, "ask stream opening");
    let request = AskRequest {
        question,
        account_id,
        ..AskRequest::default()
    };
    // A deadline on *opening* the stream, unlike `WatchEvents`: a quiet event
    // stream is working correctly, whereas an ask that never starts leaves
    // the pane on "answering…" with nothing to say why. The stream itself is
    // deliberately unbounded once open — a long answer is a long answer.
    let opened = tokio::time::timeout(RPC_TIMEOUT, client.ask_mailbox(request)).await;
    let mut stream = match opened {
        Ok(Ok(response)) => response.into_inner(),
        Ok(Err(status)) => {
            let _ = out.send(Msg::Ask {
                generation,
                event: AskEvent::Failed(status.message().to_owned()),
            });
            return;
        }
        Err(_) => {
            let _ = out.send(Msg::Ask {
                generation,
                event: AskEvent::Failed(format!(
                    "the daemon did not start answering within {}s",
                    RPC_TIMEOUT.as_secs()
                )),
            });
            return;
        }
    };
    let mut finished = false;
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(chunk) => match chunk.body {
                Some(ask_chunk::Body::Trace(trace)) => AskEvent::Trace(wire::ask_trace(&trace)),
                Some(ask_chunk::Body::Token(token)) => AskEvent::Token(token),
                Some(ask_chunk::Body::Citation(citation)) => {
                    AskEvent::Cite(Box::new(wire::citation(citation)))
                }
                Some(ask_chunk::Body::Done(done)) => {
                    finished = true;
                    AskEvent::Done {
                        grounded: done.grounded,
                        refusal: done.refusal,
                    }
                }
                // Live token accounting; the durable, billed copy is the
                // audit ledger's, not this echo's.
                Some(ask_chunk::Body::Usage(_)) | None => continue,
            },
            Err(status) => {
                finished = true;
                AskEvent::Failed(status.message().to_owned())
            }
        };
        if out.send(Msg::Ask { generation, event }).is_err() {
            return;
        }
        if finished {
            return;
        }
    }
    let _ = out.send(Msg::Ask {
        generation,
        event: AskEvent::Failed("the daemon ended the answer early".to_owned()),
    });
}

/// Drive one `ComposeService.DraftReply` stream into the model. The same
/// shape [`stream_ask`] is: a deadline on *opening* only, frames mapped in
/// the proto's own order (context, then tokens, then the draft, then done),
/// and a stream that ends with no `done` frame reported as failed rather than
/// left to read as a complete reply.
async fn stream_draft_reply(
    client: &mut ComposeServiceClient<Conn>,
    message_id: i64,
    intent: String,
    reply_all: bool,
    generation: u64,
    out: &UnboundedSender<Msg>,
) {
    tracing::debug!(generation, message_id, "draft-reply stream opening");
    let request = DraftReplyRequest {
        message_id,
        intent,
        reply_all,
    };
    let opened = tokio::time::timeout(RPC_TIMEOUT, client.draft_reply(request)).await;
    let mut stream = match opened {
        Ok(Ok(response)) => response.into_inner(),
        Ok(Err(status)) => {
            let _ = out.send(Msg::Reply {
                generation,
                event: ReplyEvent::Failed(status.message().to_owned()),
            });
            return;
        }
        Err(_) => {
            let _ = out.send(Msg::Reply {
                generation,
                event: ReplyEvent::Failed(format!(
                    "the daemon did not start drafting within {}s",
                    RPC_TIMEOUT.as_secs()
                )),
            });
            return;
        }
    };
    let mut finished = false;
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(chunk) => match chunk.event {
                Some(draft_reply_event::Event::Context(context)) => {
                    ReplyEvent::Context(wire::draft_reply_context(&context))
                }
                Some(draft_reply_event::Event::Token(token)) => ReplyEvent::Token(token),
                Some(draft_reply_event::Event::Draft(draft)) => ReplyEvent::Drafted {
                    draft_id: draft.id,
                    to: wire::addr_list(&draft.to),
                },
                Some(draft_reply_event::Event::Done(_)) => {
                    finished = true;
                    ReplyEvent::Done
                }
                // Live token accounting; the durable, billed copy is the
                // audit ledger's, not this echo's — the same reason
                // `stream_ask` skips `ask_chunk::Body::Usage`.
                Some(draft_reply_event::Event::Usage(_)) | None => continue,
            },
            Err(status) => {
                finished = true;
                ReplyEvent::Failed(status.message().to_owned())
            }
        };
        if out.send(Msg::Reply { generation, event }).is_err() {
            return;
        }
        if finished {
            return;
        }
    }
    let _ = out.send(Msg::Reply {
        generation,
        event: ReplyEvent::Failed("the daemon ended the reply early".to_owned()),
    });
}

impl GrpcExec {
    /// Run a unary request and deliver its rows as one complete Report frame.
    ///
    /// Through the superseding slot for the reason `Cmd::AuthStatus` explains:
    /// `Esc` needs one thing to abort whichever kind of report is running, and
    /// `r` supersedes by *issuing* rather than by cancelling, which only works
    /// if the previous request is in a slot the new one replaces.
    fn report<F>(&self, generation: u64, out: UnboundedSender<Msg>, work: F)
    where
        F: Future<Output = Result<Vec<ReportRow>, String>> + Send + 'static,
    {
        self.spawn_superseding(&self.reporting, async move {
            let event = match work.await {
                Ok(rows) => ReportEvent::Frame {
                    fill: ReportFill::Replace,
                    rows,
                    complete: true,
                },
                Err(error) => ReportEvent::Failed(error),
            };
            let _ = out.send(Msg::Report { generation, event });
        });
    }

    /// Run a streaming request, handing it a sink that stamps every frame.
    ///
    /// The sink exists so a stream's own loop cannot forget the generation: it
    /// takes rows and a `complete` flag and nothing else, so there is no way to
    /// send an unstamped frame from inside one.
    fn stream_report<F, Fut>(&self, generation: u64, out: UnboundedSender<Msg>, work: F)
    where
        F: FnOnce(ReportSink) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        self.spawn_superseding(&self.reporting, async move {
            work(ReportSink { generation, out }).await;
        });
    }
}

/// Where a streamed report's frames go.
struct ReportSink {
    generation: u64,
    out: UnboundedSender<Msg>,
}

impl ReportSink {
    /// One frame of rows. A snapshot, so it replaces — see
    /// `wire::index_progress_rows`.
    fn rows(&self, rows: Vec<ReportRow>, complete: bool) {
        let _ = self.out.send(Msg::Report {
            generation: self.generation,
            event: ReportEvent::Frame {
                fill: ReportFill::Replace,
                rows,
                complete,
            },
        });
    }

    /// The stream failed. Rows already delivered are kept by the pane.
    fn failed(&self, error: String) {
        let _ = self.out.send(Msg::Report {
            generation: self.generation,
            event: ReportEvent::Failed(error),
        });
    }
}

/// Drain an `IndexProgress` stream into a Report.
///
/// The last frame the daemon sends carries `done`, and it is what completes the
/// report. A stream that ends *without* one is reported as a failure rather than
/// as a finished pass: `Reindex` and `Rebuild` both promise a terminal frame, so
/// its absence means the connection went away mid-pass and the counters on
/// screen are not a total.
async fn drain_progress<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<IndexProgress>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(progress)) => {
                let done = progress.done;
                sink.rows(wire::index_progress_rows(&progress), done);
                if done {
                    return;
                }
            }
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.failed("the daemon ended the pass early".to_owned()),
        }
    }
}

/// Drain an `AnalyzeMessage` stream into a Report.
///
/// The prose the model streams is deliberately *not* drawn: this verb exists to
/// drive the pipeline over one message and say whether it worked, and the
/// analysis itself is what the AI panel shows once it is cached. So the report
/// counts tokens as they arrive — which is the only visible sign a model call is
/// alive — and the terminal frame reports what it cost.
async fn analyze<S>(request: S, sink: ReportSink)
where
    S: Future<
        Output = Result<
            tonic::Response<tonic::Streaming<rmail_proto::v1::AnalyzeEvent>>,
            tonic::Status,
        >,
    >,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    let mut tokens = 0_u64;
    let mut tools = 0_u64;
    loop {
        let row = |state: &str, tokens: u64, tools: u64, complete: bool| {
            vec![
                ReportRow::new(["state", state]).toned(if complete {
                    ReportTone::Ok
                } else {
                    ReportTone::Muted
                }),
                ReportRow::new(["tokens", &tokens.to_string()]),
                ReportRow::new(["tool calls", &tools.to_string()]),
            ]
        };
        match stream.next().await {
            Some(Ok(event)) => match event.event {
                Some(analyze_event::Event::Token(_)) => {
                    tokens += 1;
                    sink.rows(row("analysing…", tokens, tools, false), false);
                }
                Some(analyze_event::Event::ToolUseStart(_)) => {
                    tools += 1;
                    sink.rows(row("analysing…", tokens, tools, false), false);
                }
                Some(analyze_event::Event::Usage(usage)) => {
                    // Tokens the *provider* counted, which is not the same
                    // number as the frames streamed above — a token frame can
                    // carry several tokens' worth of text. Reported as its own
                    // row rather than replacing the count, so neither figure is
                    // presented as the other.
                    let mut rows = row("analysing…", tokens, tools, false);
                    rows.push(ReportRow::new([
                        "billed tokens".to_owned(),
                        (usage.input_tokens
                            + usage.output_tokens
                            + usage.cache_creation_input_tokens
                            + usage.cache_read_input_tokens)
                            .to_string(),
                    ]));
                    sink.rows(rows, false);
                }
                Some(analyze_event::Event::Done(_)) => {
                    return sink.rows(row("analysed", tokens, tools, true), true);
                }
                // A frame this build does not know: counted as progress rather
                // than dropped silently, so a newer daemon's extra event kind
                // still reads as "something is happening".
                None => sink.rows(row("analysing…", tokens, tools, false), false),
            },
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.failed("the daemon ended the analysis early".to_owned()),
        }
    }
}

/// What a pause verb says while it is outstanding.
///
/// One place, so `indexer`, `sync` and `AI dispatch` cannot end up phrased three
/// ways for the same operation.
fn paused_label(what: &str, pause: commands::Pause) -> String {
    if pause.paused() {
        format!("{what} stopped")
    } else {
        format!("{what} started")
    }
}

/// One round of the heartbeat: ask the four subsystems and report each one.
///
/// Nothing here touches `inflight`, in either direction. That counter is what
/// the busy marker reads and it means "work the user asked for"; a five-second
/// poll incrementing it would pin the marker on forever, and decrementing it
/// would drive it below zero on the first tick.
///
/// A failure is reported as a failure rather than dropped: a daemon that has
/// gone away is exactly what the indicator zone exists to show, and silence
/// would leave the last healthy answer on screen indefinitely.
async fn heartbeat(
    sync: &mut SyncServiceClient<Conn>,
    index: &mut IndexServiceClient<Conn>,
    ai: &mut AiServiceClient<Conn>,
    policy: &mut AiPolicyServiceClient<Conn>,
    account_id: i64,
    out: &UnboundedSender<Msg>,
) {
    let report = |subsystem: Subsystem, result: Result<Health, String>| {
        let _ = out.send(Msg::Daemon { subsystem, result });
    };
    report(
        Subsystem::Sync,
        call(sync.status(SyncStatusRequest { account_id }))
            .await
            .map(|response| wire::sync_health(&response.into_inner())),
    );
    report(
        Subsystem::Index,
        call(index.status(IndexStatusRequest {}))
            .await
            .map(|response| wire::index_health(&response.into_inner())),
    );
    report(
        Subsystem::Ai,
        call(ai.get_usage(GetUsageRequest {}))
            .await
            .map(|response| wire::ai_health(&response.into_inner())),
    );
    report(
        Subsystem::Spend,
        // The account's own budget rather than the global one: the bar is
        // about the mailbox on screen, and `GetSpend` treats 0 as "every call
        // whichever account made it", which is a different question.
        call(policy.get_spend(GetSpendRequest { account_id }))
            .await
            .map(|response| wire::spend_health(&response.into_inner())),
    );
}

/// The `:auth status` report: the daemon's gate, then this client's own
/// credential.
///
/// Two frames from two sources, which is what [`ReportFill`] is for. The
/// daemon's two settings are the complete current state of the gate, so that
/// frame *replaces*; the credential row comes from argv and the keychain and
/// must not erase what the daemon said, so it *appends*. It is also sent when
/// the RPC failed, because "the daemon did not answer and I am presenting
/// nothing" is the most useful thing this screen can say — and it is sent
/// before the failure frame, since [`ReportEvent::Failed`] ends the report.
async fn auth_status(
    client: &mut ClientAuthServiceClient<Conn>,
    socket: &Path,
    generation: u64,
    out: &UnboundedSender<Msg>,
) {
    let answered = match call(client.auth_status(AuthStatusRequest {})).await {
        Ok(response) => {
            let _ = out.send(Msg::Report {
                generation,
                event: ReportEvent::Frame {
                    fill: ReportFill::Replace,
                    rows: wire::auth_status_rows(&response.into_inner()),
                    complete: false,
                },
            });
            Ok(())
        }
        Err(error) => Err(error),
    };
    let credential = crate::client::credential(&crate::client::current_transport(), socket).await;
    let _ = out.send(Msg::Report {
        generation,
        event: ReportEvent::Frame {
            fill: ReportFill::Append,
            rows: vec![credential_row(&credential)],
            complete: answered.is_ok(),
        },
    });
    if let Err(error) = answered {
        let _ = out.send(Msg::Report {
            generation,
            event: ReportEvent::Failed(error),
        });
    }
}

/// The row naming which credential this client would present — the kind, never
/// the secret.
///
/// Here rather than in `wire` because a credential is not a wire type: it comes
/// from argv and the keychain, and `wire`'s job is proto to model. This is the
/// module that has both halves of the `:auth status` answer.
///
/// A cached session or an explicit `--token` is the state everything works in,
/// so both read [`ReportTone::Ok`]; presenting nothing is [`ReportTone::Muted`]
/// rather than a warning, because over the Unix socket with `local login` off it
/// is the normal, correct state — the daemon's own row above is where "and that
/// is not enough here" is said.
fn credential_row(credential: &crate::client::Credential) -> ReportRow {
    let tone = match credential {
        crate::client::Credential::Flag(_) | crate::client::Credential::Cached(_) => ReportTone::Ok,
        crate::client::Credential::None => ReportTone::Muted,
    };
    ReportRow::new(["this client presents", credential.describe()]).toned(tone)
}

/// `ClientAuthService.ClearPassword`, plus the local session hygiene
/// `mail auth clear` performs.
///
/// Refused under `--addr` for exactly the reason `auth_cli::run` refuses it:
/// `crate::session` is keyed by the *local* socket path and has no `--addr`
/// form, so clearing against a remote daemon would forget a session belonging
/// to the local one. Two surfaces over one capability that disagreed about
/// that would be the drift `rmail_core::parity` exists to prevent.
///
/// Forgetting the session is best effort: the password is already gone at the
/// daemon, which is the part that matters, and a keychain that refuses to
/// delete must not turn a completed operation into a reported failure.
async fn clear_password(
    client: &mut ClientAuthServiceClient<Conn>,
    socket: &Path,
) -> Result<Effect, String> {
    if let Some(addr) = crate::client::remote_addr() {
        return Err(format!(
            "`:auth clear` manages the local session cache, which is keyed by socket path — it \
             cannot be pointed at --addr {addr}"
        ));
    }
    call(client.clear_password(ClearPasswordRequest {})).await?;
    // `spawn_blocking` for the reason `client::credential` reads the same store
    // that way: the Keychain API is synchronous FFI that can raise an OS access
    // prompt, and waiting on a human inside an async task blocks the runtime
    // thread the whole TUI is drawn from.
    let path = socket.to_path_buf();
    match tokio::task::spawn_blocking(move || crate::session::clear(&path)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            error = %error,
            "the password was cleared, but the cached session could not be forgotten",
        ),
        Err(error) => tracing::warn!(
            error = %error,
            "the password was cleared, but the task forgetting the cached session did not finish",
        ),
    }
    Ok(Effect::None)
}

async fn list_outbox(
    client: &mut SendSchedulerServiceClient<Conn>,
    account_id: i64,
) -> Result<Vec<super::overlays::OutboxRow>, String> {
    let response = call(client.list_outbox(ListOutboxRequest {
        account_id: Some(account_id),
        page_size: OUTBOX_PAGE,
        ..ListOutboxRequest::default()
    }))
    .await?;
    Ok(response
        .into_inner()
        .entries
        .into_iter()
        .map(wire::outbox_row)
        .collect())
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
    client: &mut MailServiceClient<Conn>,
    mailbox_id: i64,
) -> Result<Vec<super::model::MessageRow>, String> {
    let response = call(client.list(ListMessagesRequest {
        mailbox_id,
        page_size: PAGE_SIZE,
        // The TUI shows one page; paging through it is task 42's surface.
        page_token: String::new(),
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
    client: &mut MailServiceClient<Conn>,
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
