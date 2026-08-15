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
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::finder_service_client::FinderServiceClient;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    ask_chunk, AskRequest, CancelRequest, CopyRequest, DeleteRequest, EventKind, ExplainRequest,
    FindRequest, GetMessageRequest, GetSummaryRequest, ListAccountsRequest, ListMessagesRequest,
    ListOutboxRequest, MoveRequest, SearchRequest, SetFlagsRequest, SuggestReplyRequest,
    SyncStatusRequest, WatchEventsRequest,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::transport::Channel;

use super::html::{self, CommandOpener};
use super::model::drive::CmdExec;
use super::model::{wire, AskEvent, Cmd, Effect, FinderEvent, Msg, SearchEvent, Stream};

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

/// Runs the TUI's commands against a live `rmaild`.
pub struct GrpcExec {
    mail: MailServiceClient<Channel>,
    sync: SyncServiceClient<Channel>,
    accounts: AccountServiceClient<Channel>,
    compose: ComposeServiceClient<Channel>,
    search: SearchServiceClient<Channel>,
    finder: FinderServiceClient<Channel>,
    ai: AiServiceClient<Channel>,
    scheduler: SendSchedulerServiceClient<Channel>,
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
    /// The why-panel's `Explain`. A slot even though the RPC is unary:
    /// holding `j` down the results issues one per row, each re-running the
    /// whole retrieval pipeline server-side, and only the last one can ever
    /// be drawn.
    explaining: Mutex<Option<AbortHandle>>,
    ticking: Mutex<Option<AbortHandle>>,
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

    /// Build every client over an already-established channel.
    #[must_use]
    pub fn with_channel(channel: Channel) -> Self {
        let cancel = CancellationToken::new();
        Self {
            mail: MailServiceClient::new(channel.clone()),
            sync: SyncServiceClient::new(channel.clone()),
            accounts: AccountServiceClient::new(channel.clone()),
            compose: ComposeServiceClient::new(channel.clone()),
            search: SearchServiceClient::new(channel.clone()),
            finder: FinderServiceClient::new(channel.clone()),
            ai: AiServiceClient::new(channel.clone()),
            scheduler: SendSchedulerServiceClient::new(channel),
            searching: Mutex::new(None),
            finding: Mutex::new(None),
            asking: Mutex::new(None),
            explaining: Mutex::new(None),
            ticking: Mutex::new(None),
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
            Cmd::CancelStream { which } => {
                let slot = match which {
                    Stream::Search => &self.searching,
                    Stream::Find => &self.finding,
                    Stream::Ask => &self.asking,
                    Stream::Explain => &self.explaining,
                };
                abort(slot);
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
    client: &mut SearchServiceClient<Channel>,
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
    client: &mut FinderServiceClient<Channel>,
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
    client: &mut AiServiceClient<Channel>,
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

async fn list_outbox(
    client: &mut SendSchedulerServiceClient<Channel>,
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
    client: &mut MailServiceClient<Channel>,
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
