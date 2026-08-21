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
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use rmail_core::export::write::DestinationWriter;
use rmail_core::export::{Chunk as ExportPart, Format as ArchiveFormat};
use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::ai_policy_service_client::AiPolicyServiceClient;
use rmail_proto::v1::ai_safety_service_client::AiSafetyServiceClient;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::attachment_service_client::AttachmentServiceClient;
use rmail_proto::v1::audit_service_client::AuditServiceClient;
use rmail_proto::v1::client_auth_service_client::ClientAuthServiceClient;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::export_service_client::ExportServiceClient;
use rmail_proto::v1::extract_service_client::ExtractServiceClient;
use rmail_proto::v1::finder_service_client::FinderServiceClient;
use rmail_proto::v1::hook_service_client::HookServiceClient;
use rmail_proto::v1::index_service_client::IndexServiceClient;
use rmail_proto::v1::link_service_client::LinkServiceClient;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::note_service_client::NoteServiceClient;
use rmail_proto::v1::notification_service_client::NotificationServiceClient;
use rmail_proto::v1::rule_service_client::RuleServiceClient;
use rmail_proto::v1::saved_search_service_client::SavedSearchServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::tag_service_client::TagServiceClient;
use rmail_proto::v1::webhook_service_client::WebhookServiceClient;
use rmail_proto::v1::{
    analyze_event, ask_chunk, bulk_tag_request, draft_reply_event, export_request, target,
    AddNoteRequest, AddTagRequest, AiProviderKind, Alert, AnalyzeMessageRequest,
    AskAnalyticsRequest, AskAttachmentChunk, AskAttachmentRequest, AskRequest, AuditEntry,
    AuditFilter, AuthStatusRequest, AutoconfigureRequest, BacktestRuleRequest, BeginOAuthRequest,
    BudgetCaps, BudgetClass, BudgetWindowCaps, BulkTagRequest, CallStatus, CancelRequest,
    ClearPasswordRequest, CompileQueryRequest, CompileSmartFolderRequest, CompleteOAuthRequest,
    ConfirmInjectionRequest, CopyRequest, CreateAccountRequest, CreateFollowupRequest,
    CreateRuleRequest, CreateSavedSearchRequest, CreateSmartFolderRequest, CreateTagRequest,
    CredentialRef, DeleteAccountRequest, DeleteDraftRequest, DeleteNoteRequest, DeleteRequest,
    DeleteSavedSearchRequest, DeleteSmartFolderRequest, DraftNudgeRequest, DraftReplyRequest,
    EditNoteRequest, EvaluateRequest, EvaluateRulesRequest, EvaluateSmartFolderRequest, EventKind,
    ExplainRequest, ExportChunk, ExportFormat, ExportInvoicesRequest, ExportLedgerRequest,
    ExportRequest, ExtractEventsRequest, ExtractInvoiceRequest, ExtractLinksRequest,
    ExtractStructuredRequest, ExtractTablesRequest, ExtractTasksRequest, ExtractionSink,
    FindRequest, FinderRebuildRequest, FinderStatusRequest, ForwardMessageRequest,
    GenerateDigestRequest, GetAccountRequest, GetAiProviderRequest, GetContactInsightRequest,
    GetDraftRequest, GetMessageRequest, GetResponseTimesRequest, GetSpendRequest,
    GetSummaryRequest, GetUsageRequest, GoldenQuery as WireGoldenQuery, IdRequest, IndexGcRequest,
    IndexProgress, IndexStatusRequest, InvoiceExportFormat, Judgment as WireJudgment,
    ListAccountsRequest, ListDeliveriesRequest, ListDraftRevisionsRequest, ListDraftsRequest,
    ListEntitiesRequest, ListFollowupsRequest, ListHooksRequest, ListMessagesRequest,
    ListNotesRequest, ListOutboxRequest, ListRulesRequest, ListSavedSearchesRequest,
    ListSmartFolderMembersRequest, ListSmartFoldersRequest, ListSubscriptionsRequest,
    ListTagRulesRequest, ListTagsRequest, ListTokensRequest, ListWaitingOnRequest,
    ListWebhooksRequest, Message as ProtoMessage, MintTokenRequest, Mode as EvalMode, MoveRequest,
    NoteAuthor, NoteEvent as WireNoteEvent, NoteTarget, PauseRequest, PreflightCheckRequest,
    QueryAiCallsRequest, RebuildRequest, RecordCorrectionRequest, RefreshTokenRequest,
    RegisterWebhookRequest, ReindexMode, ReindexRequest, RemoveTagRequest, RemoveWebhookRequest,
    RenderDraftRequest, ReplayDeliveryRequest, RescheduleRequest, ResolveSuggestionRequest,
    ResponseTimeGroupBy, ResumeRequest, RetryFailedRequest, RevokeTokenRequest,
    RewriteDraftRequest, RunSavedSearchRequest, ScanInjectionRequest, ScheduleSendRequest,
    ScoreMessageRequest, SearchAttachmentsRequest, SearchEntitiesRequest, SearchHit, SearchRequest,
    SelectDraftRevisionRequest, SetAiProviderRequest, SetBudgetRequest, SetFlagsRequest,
    SetIndexPausedRequest, SetPausedRequest, SetTagRuleRequest, SetWebhookEnabledRequest,
    StreamAlertsRequest, SuggestReplyRequest, SuggestSendTimeRequest, SuggestTagsRequest,
    SyncFolderRequest, SyncMode, SyncStatusRequest, SynthesizeRuleRequest, TagRuleMode,
    TagSuggestion, TagSyncMode, Target, TestConnectionRequest, TestHookRequest, UpdateBodyRequest,
    UpdateDraftRequest, UpdateSavedSearchRequest, VerifyIndexRequest, WatchEventsRequest,
    WatchNotesRequest, WebhookSecretSource, WebhookTemplate,
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
use super::config_block::{ConfigBlock, ReadOnlyReason};
use super::history;
use super::html::{self, CommandOpener};
use super::ledger;
use super::model::drive::CmdExec;
use super::model::{
    wire, write_keybinding, AskEvent, Cmd, Credential, Effect, FinderEvent, FormEvent, Msg,
    ReplyEvent, ReportEvent, SearchEvent, Stream,
};
use super::report::{ReportFill, ReportRow, ReportTone};
use super::status::{Health, Subsystem};

/// How many export frames may sit between the gRPC stream and the writer task.
///
/// Small on purpose, for the reason `export_cli`'s own constant is: the point is
/// to keep the disk and the socket coupled, so a slow disk throttles the daemon's
/// scan instead of letting this process buffer an archive it has not written.
const EXPORT_QUEUE: usize = 4;

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

/// How many pending ledger deltas `watch()` accumulates before flushing
/// early, ahead of the usual `COALESCE` tick. Ordinary bursts stay well
/// under this and flush on schedule; a fresh subscription's `since_seq: 0`
/// backlog replay does not — a large mailbox can hand the stream thousands
/// of events inside one 300&nbsp;ms window, and holding all of them (each a
/// `Delta::Flags` owning its own `Vec<String>`) until the tick would let
/// that one buffer grow with no bound but the backlog's own size. Bounds
/// that one `Vec`'s growth, matching `PAGE_SIZE` for the same reason a
/// folder listing does — not a cap on total outstanding memory, since `out`
/// is an unbounded channel and a slow-draining consumer can still queue up
/// more than one flushed batch behind it.
const DELTA_BATCH: usize = 500;

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
    safety: AiSafetyServiceClient<Conn>,
    audit: AuditServiceClient<Conn>,
    admin: AdminServiceClient<Conn>,
    webhooks: WebhookServiceClient<Conn>,
    export: ExportServiceClient<Conn>,
    analytics: AnalyticsServiceClient<Conn>,
    attachments: AttachmentServiceClient<Conn>,
    extract: ExtractServiceClient<Conn>,
    links: LinkServiceClient<Conn>,
    notes: NoteServiceClient<Conn>,
    saved: SavedSearchServiceClient<Conn>,
    hooks: HookServiceClient<Conn>,
    notify: NotificationServiceClient<Conn>,
    tags: TagServiceClient<Conn>,
    rules: RuleServiceClient<Conn>,
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
    /// `MailService.WatchEvents`. Superseding for the reason `beating` is, and
    /// since task 97 that matters: `:account use` re-issues `Cmd::Watch`, and a
    /// plain spawn would leave one open stream per switch — each still sending
    /// `Msg::Changed` for an account nobody is looking at.
    watching: Mutex<Option<AbortHandle>>,
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
            policy: AiPolicyServiceClient::new(channel.clone()),
            safety: AiSafetyServiceClient::new(channel.clone()),
            audit: AuditServiceClient::new(channel.clone()),
            admin: AdminServiceClient::new(channel.clone()),
            webhooks: WebhookServiceClient::new(channel.clone()),
            export: ExportServiceClient::new(channel.clone()),
            analytics: AnalyticsServiceClient::new(channel.clone()),
            attachments: AttachmentServiceClient::new(channel.clone()),
            extract: ExtractServiceClient::new(channel.clone()),
            links: LinkServiceClient::new(channel.clone()),
            notes: NoteServiceClient::new(channel.clone()),
            saved: SavedSearchServiceClient::new(channel.clone()),
            hooks: HookServiceClient::new(channel.clone()),
            notify: NotificationServiceClient::new(channel.clone()),
            tags: TagServiceClient::new(channel.clone()),
            rules: RuleServiceClient::new(channel),
            searching: Mutex::new(None),
            finding: Mutex::new(None),
            asking: Mutex::new(None),
            replying: Mutex::new(None),
            explaining: Mutex::new(None),
            reporting: Mutex::new(None),
            beating: Mutex::new(None),
            watching: Mutex::new(None),
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
            Cmd::TagList {
                generation,
                account_id,
            } => {
                let mut client = self.tags.clone();
                self.report(generation, out, async move {
                    call(client.list_tags(ListTagsRequest { account_id }))
                        .await
                        .map(|r| wire::tag_rows(&r.into_inner()))
                });
            }
            Cmd::TagApply {
                generation,
                message_ids,
                name,
                remove,
            } => {
                let mut client = self.tags.clone();
                self.stream_report(generation, out, move |sink| async move {
                    apply_tags(&mut client, &message_ids, &name, remove, &sink).await;
                });
            }
            Cmd::TagCreate {
                account_id,
                name,
                color,
                sync,
            } => {
                let mut client = self.tags.clone();
                self.spawn(out, async move {
                    let label = format!("tag {name} created");
                    Msg::Done {
                        label,
                        result: call(client.create_tag(CreateTagRequest {
                            account_id,
                            name,
                            color,
                            sync_mode: sync.map(|sync| tag_sync_mode(sync) as i32),
                            parent_id: None,
                        }))
                        .await
                        .map(|_| Effect::None),
                    }
                });
            }
            Cmd::TagBulk {
                generation,
                account_id,
                query,
                name,
            } => {
                let mut client = self.tags.clone();
                self.report(generation, out, async move {
                    call(client.bulk_tag(BulkTagRequest {
                        account_id,
                        selector: Some(bulk_tag_request::Selector::Query(query)),
                        names: vec![name],
                    }))
                    .await
                    .map(|r| wire::tag_bulk_rows(&r.into_inner()))
                });
            }
            Cmd::TagSuggest {
                generation,
                message_id,
            } => {
                let mut client = self.tags.clone();
                self.stream_report(generation, out, move |sink| async move {
                    stream_suggestions(
                        client.suggest_tags(SuggestTagsRequest { message_id }),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::TagResolve {
                message_tag_id,
                resolve,
            } => {
                let mut client = self.tags.clone();
                self.spawn(out, async move {
                    Msg::Done {
                        label: match resolve {
                            commands::tag::Resolve::Accept => "suggestion accepted".to_owned(),
                            commands::tag::Resolve::Reject => "suggestion rejected".to_owned(),
                        },
                        result: call(client.resolve_suggestion(ResolveSuggestionRequest {
                            message_tag_id,
                            accept: resolve.accept(),
                        }))
                        .await
                        .map(|_| Effect::None),
                    }
                });
            }
            Cmd::TagRules {
                generation,
                account_id,
            } => {
                let mut client = self.tags.clone();
                self.report(generation, out, async move {
                    call(client.list_tag_rules(ListTagRulesRequest { account_id }))
                        .await
                        .map(|r| wire::tag_rule_rows(&r.into_inner()))
                });
            }
            Cmd::TagRuleSet {
                account_id,
                name,
                tag,
                mode,
                min_conf_pct,
                enabled,
            } => {
                let mut client = self.tags.clone();
                self.spawn(out, async move {
                    let label = format!("tag rule {name} stored");
                    Msg::Done {
                        label,
                        result: call(client.set_tag_rule(SetTagRuleRequest {
                            account_id,
                            name,
                            tag_name: tag,
                            mode: match mode {
                                commands::tag::RuleMode::Suggest => TagRuleMode::Suggest as i32,
                                commands::tag::RuleMode::Auto => TagRuleMode::Auto as i32,
                            },
                            // Back to a fraction at the wire seam, which is the
                            // only place it has to be one — see
                            // `commands::tag::percent` on why the `Cmd` carries
                            // whole percent instead.
                            min_conf: f64::from(min_conf_pct) / 100.0,
                            enabled,
                        }))
                        .await
                        .map(|_| Effect::None),
                    }
                });
            }
            Cmd::RuleList {
                generation,
                account_id,
            } => {
                let mut client = self.rules.clone();
                self.report(generation, out, async move {
                    call(client.list_rules(ListRulesRequest { account_id }))
                        .await
                        .map(|r| wire::rule_rows(&r.into_inner()))
                });
            }
            Cmd::RuleSynthesize {
                generation,
                account_id,
                instruction,
                days,
            } => {
                let mut client = self.rules.clone();
                let reporter = out.clone();
                self.report(generation, out, async move {
                    let response = call(client.synthesize_rule(SynthesizeRuleRequest {
                        account_id,
                        instruction,
                        days: days.unwrap_or(0),
                    }))
                    .await?
                    .into_inner();
                    // The draft travels back to the model as well as to the
                    // report, because `:rule add` stores it and a document that
                    // only ever existed inside a rendered row could not be
                    // stored at all.
                    let _ = reporter.send(Msg::RuleDrafted(response.toml.clone()));
                    Ok(wire::rule_draft_rows(&response))
                });
            }
            Cmd::RuleCreate { account_id, toml } => {
                let mut client = self.rules.clone();
                self.spawn(out, async move {
                    let result = call(client.create_rule(CreateRuleRequest { account_id, toml }))
                        .await
                        .map(|response| {
                            response
                                .into_inner()
                                .rule
                                .map_or_else(|| "the rule".to_owned(), |rule| rule.name)
                        });
                    match result {
                        Ok(name) => Msg::Done {
                            label: format!("rule {name} stored"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "rule add".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::RuleEvaluate {
                generation,
                account_id,
                message_ids,
                rule,
            } => {
                let mut client = self.rules.clone();
                self.report(generation, out, async move {
                    call(client.evaluate_rules(EvaluateRulesRequest {
                        account_id,
                        message_ids,
                        rule_names: rule.into_iter().collect(),
                    }))
                    .await
                    .map(|r| {
                        let response = r.into_inner();
                        wire::rule_outcome_rows(&response.messages, response.stats.as_ref(), None)
                    })
                });
            }
            Cmd::RuleBacktest {
                generation,
                account_id,
                name,
                days,
            } => {
                let mut client = self.rules.clone();
                self.report(generation, out, async move {
                    call(client.backtest_rule(BacktestRuleRequest {
                        account_id,
                        rule_name: name,
                        // Empty, because this backtests a rule the daemon already
                        // has: `rule_toml` is for testing a document that is not
                        // stored yet, which is `:rule new`'s own dry run.
                        rule_toml: String::new(),
                        days: days.unwrap_or(0),
                    }))
                    .await
                    .map(|r| {
                        let response = r.into_inner();
                        let window = response.window_days;
                        wire::rule_outcome_rows(
                            &response.messages,
                            response.stats.as_ref(),
                            Some(window),
                        )
                    })
                });
            }
            Cmd::RuleCorrect {
                account_id,
                message_id,
                prompt,
                expected,
            } => {
                let mut client = self.rules.clone();
                self.spawn(out, async move {
                    let result = call(client.record_correction(RecordCorrectionRequest {
                        account_id,
                        message_id,
                        prompt,
                        expected,
                    }))
                    .await;
                    match result {
                        // The example count is the whole answer: it is how
                        // somebody knows whether a criterion has enough
                        // corrections behind it to have changed.
                        Ok(response) => Msg::Done {
                            label: format!(
                                "correction recorded — {} example(s) for that criterion",
                                response.into_inner().example_count
                            ),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "rule correct".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }

            // -- content, export and analytics (task 99) ----------------------
            Cmd::Export {
                generation,
                query,
                thread_id,
                format,
                to,
                with_ai,
                limit,
            } => {
                let mut client = self.export.clone();
                self.stream_report(generation, out, move |sink| async move {
                    run_export(
                        &mut client,
                        ExportRequest {
                            selection: Some(match thread_id {
                                Some(thread_id) => export_request::Selection::ThreadId(thread_id),
                                None => export_request::Selection::Query(query),
                            }),
                            format: export_format(format) as i32,
                            with_ai,
                            limit: i32::try_from(limit.unwrap_or(0)).unwrap_or(i32::MAX),
                        },
                        format,
                        PathBuf::from(&to),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::ResponseTimes {
                generation,
                account_id,
                group_by,
                since_secs,
                until,
                limit,
                min_samples,
            } => {
                let mut client = self.analytics.clone();
                self.report(generation, out, async move {
                    let (since, until) = window(since_secs, until);
                    call(client.get_response_times(GetResponseTimesRequest {
                        account_id,
                        group_by: match group_by {
                            commands::content::analytics::GroupBy::Contact => {
                                ResponseTimeGroupBy::Contact
                            }
                            commands::content::analytics::GroupBy::Mailbox => {
                                ResponseTimeGroupBy::Mailbox
                            }
                        } as i32,
                        since,
                        until,
                        bucket_seconds: 0,
                        window_seconds: 0,
                        limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                        min_samples: u32::try_from(min_samples.unwrap_or(0)).unwrap_or(u32::MAX),
                        bottleneck_ratio: 0.0,
                    }))
                    .await
                    .map(|r| wire::response_time_rows(&r.into_inner()))
                });
            }
            Cmd::AskAnalytics {
                generation,
                account_id,
                question,
                narrate,
            } => {
                let mut client = self.analytics.clone();
                self.report(generation, out, async move {
                    call(client.ask_analytics(AskAnalyticsRequest {
                        account_id,
                        question,
                        narrate,
                    }))
                    .await
                    .map(|r| wire::ask_analytics_rows(&r.into_inner()))
                });
            }
            Cmd::Digest {
                generation,
                account_id,
                since_secs,
                until,
                force,
            } => {
                let mut client = self.analytics.clone();
                self.report(generation, out, async move {
                    let (since, until) = window(since_secs, until);
                    call(client.generate_digest(GenerateDigestRequest {
                        account_id,
                        since,
                        until,
                        force,
                    }))
                    .await
                    .map(|r| wire::digest_rows(&r.into_inner()))
                });
            }
            Cmd::ContactInsight {
                generation,
                account_id,
                address,
                since_secs,
                until,
                metrics_only,
            } => {
                let mut client = self.analytics.clone();
                self.report(generation, out, async move {
                    let (since, until) = window(since_secs, until);
                    call(client.get_contact_insight(GetContactInsightRequest {
                        account_id,
                        address,
                        since,
                        until,
                        topic_limit: 0,
                        metrics_only,
                    }))
                    .await
                    .map(|r| wire::contact_rows(&r.into_inner()))
                });
            }
            Cmd::Subscriptions {
                generation,
                account_id,
                since_secs,
                until,
                limit,
                candidates_only,
                classify_unknown,
            } => {
                let mut client = self.analytics.clone();
                self.report(generation, out, async move {
                    let (since, until) = window(since_secs, until);
                    call(client.list_subscriptions(ListSubscriptionsRequest {
                        account_id,
                        since,
                        until,
                        limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                        candidates_only,
                        classify_unknown,
                    }))
                    .await
                    .map(|r| wire::subscription_rows(&r.into_inner()))
                });
            }
            Cmd::AttachTables {
                generation,
                message_id,
                part,
                allow_model,
            } => {
                let mut client = self.attachments.clone();
                self.report(generation, out, async move {
                    call(client.extract_tables(ExtractTablesRequest {
                        message_id,
                        part_id: part.unwrap_or_default(),
                        allow_model,
                    }))
                    .await
                    .map(|r| wire::table_rows(&r.into_inner()))
                });
            }
            Cmd::AttachInvoice {
                generation,
                message_id,
                part,
                use_model,
            } => {
                let mut client = self.attachments.clone();
                self.report(generation, out, async move {
                    call(client.extract_invoice(ExtractInvoiceRequest {
                        message_id,
                        part_id: part.unwrap_or_default(),
                        use_model,
                    }))
                    .await
                    .map(|r| wire::invoice_rows(&r.into_inner()))
                });
            }
            Cmd::AttachInvoices {
                generation,
                account_id,
                vendor,
                since_secs,
                until,
                limit,
                format,
            } => {
                let mut client = self.attachments.clone();
                self.report(generation, out, async move {
                    let (since, until) = window(since_secs, until);
                    call(client.export_invoices(ExportInvoicesRequest {
                        account_id,
                        message_id: 0,
                        vendor: vendor.unwrap_or_default(),
                        since,
                        until,
                        limit: limit.unwrap_or(0),
                        format: match format {
                            commands::content::analytics::InvoiceFormat::Rows => {
                                InvoiceExportFormat::Rows
                            }
                            commands::content::analytics::InvoiceFormat::Csv => {
                                InvoiceExportFormat::Csv
                            }
                        } as i32,
                    }))
                    .await
                    .map(|r| wire::invoices_rows(&r.into_inner()))
                });
            }
            Cmd::AttachAsk {
                generation,
                question,
                message_id,
                account_id,
                part,
                top_k,
            } => {
                let mut client = self.attachments.clone();
                self.stream_report(generation, out, move |sink| async move {
                    ask_attachment(
                        client.ask_attachment(AskAttachmentRequest {
                            question,
                            message_id,
                            part_id: part.unwrap_or_default(),
                            account_id,
                            top_k: u32::try_from(top_k.unwrap_or(0)).unwrap_or(u32::MAX),
                        }),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::AttachSearch {
                generation,
                query,
                account_id,
                message_id,
                limit,
            } => {
                let mut client = self.search.clone();
                self.report(generation, out, async move {
                    call(client.search_attachments(SearchAttachmentsRequest {
                        query,
                        account_id,
                        message_id,
                        limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                    }))
                    .await
                    .map(|r| wire::attachment_hit_rows(&r.into_inner()))
                });
            }
            Cmd::Extract {
                generation,
                message_id,
                tasks,
                use_model,
                sink,
            } => {
                let mut client = self.extract.clone();
                let sink = match sink {
                    commands::content::extract::Sink::Ics => ExtractionSink::Ics,
                    commands::content::extract::Sink::Command => ExtractionSink::Command,
                    commands::content::extract::Sink::Webhook => ExtractionSink::Webhook,
                } as i32;
                self.report(generation, out, async move {
                    if tasks {
                        return call(client.extract_tasks(ExtractTasksRequest {
                            message_id,
                            use_model,
                            sink,
                        }))
                        .await
                        .map(|r| wire::task_rows(&r.into_inner()));
                    }
                    call(client.extract_events(ExtractEventsRequest {
                        message_id,
                        use_model,
                        sink,
                    }))
                    .await
                    .map(|r| wire::event_rows(&r.into_inner()))
                });
            }
            Cmd::ExtractData {
                generation,
                message_id,
                schema,
                refresh,
            } => {
                let mut client = self.extract.clone();
                self.report(generation, out, async move {
                    call(client.extract_structured(ExtractStructuredRequest {
                        message_id,
                        schema,
                        // The schema is named, never supplied inline: a client
                        // handing the daemon a JSON schema of its own would be a
                        // second place extraction schemas live.
                        schema_json: String::new(),
                        refresh,
                    }))
                    .await
                    .map(|r| wire::structured_rows(&r.into_inner()))
                });
            }
            Cmd::Links {
                generation,
                message_id,
                use_model,
            } => {
                let mut client = self.links.clone();
                self.report(generation, out, async move {
                    call(client.extract_links(ExtractLinksRequest {
                        message_id,
                        use_model,
                    }))
                    .await
                    .map(|r| wire::link_rows(&r.into_inner()))
                });
            }
            Cmd::CompileQuery {
                generation,
                account_id,
                query,
                refresh,
            } => {
                let mut client = self.search.clone();
                self.report(generation, out, async move {
                    call(client.compile_query(CompileQueryRequest {
                        query,
                        account_id,
                        refresh,
                    }))
                    .await
                    .map(|r| wire::query_plan_rows(&r.into_inner()))
                });
            }
            Cmd::SearchEntities {
                generation,
                account_id,
                query,
                kinds,
                since_secs,
                limit,
            } => {
                let mut client = self.search.clone();
                self.report(generation, out, async move {
                    let (since, _) = window(since_secs, None);
                    call(client.search_entities(SearchEntitiesRequest {
                        query,
                        kinds,
                        account_id,
                        since,
                        limit: limit.unwrap_or(0),
                    }))
                    .await
                    .map(|r| wire::entity_rows(&r.into_inner()))
                });
            }
            Cmd::SearchEval {
                generation,
                path,
                mode,
                limit,
            } => {
                let mut client = self.search.clone();
                self.report(generation, out, async move {
                    // Read on a blocking task, and parsed by the same shared type
                    // `mail search eval` parses with — so a malformed set is
                    // refused with a message about a path the user can see rather
                    // than an INVALID_ARGUMENT about a request they did not write.
                    let set = tokio::task::spawn_blocking(move || {
                        rmail_core::eval::GoldenSet::load(Path::new(&path))
                    })
                    .await
                    .map_err(|error| format!("reading the golden set: {error}"))?
                    .map_err(|error| error.to_string())?;
                    call(
                        client.evaluate(EvaluateRequest {
                            corpus: set.corpus.clone(),
                            queries: set
                                .queries
                                .iter()
                                .map(|query| WireGoldenQuery {
                                    name: query.name.clone(),
                                    query: query.query.clone(),
                                    account_id: query.account_id,
                                    judgments: query
                                        .judgments
                                        .iter()
                                        .map(|judged| WireJudgment {
                                            message_id: judged.message_id.clone(),
                                            gain: judged.gain,
                                        })
                                        .collect(),
                                })
                                .collect(),
                            mode: match mode {
                                None => EvalMode::Unspecified,
                                Some(commands::content::extract::Mode::Lexical) => {
                                    EvalMode::Lexical
                                }
                                Some(commands::content::extract::Mode::Semantic) => {
                                    EvalMode::Semantic
                                }
                                Some(commands::content::extract::Mode::Hybrid) => EvalMode::Hybrid,
                            } as i32,
                            limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                        }),
                    )
                    .await
                    .map(|r| wire::eval_rows(&r.into_inner()))
                });
            }
            Cmd::NoteAdd {
                message_id,
                thread,
                body,
            } => {
                let mut client = self.notes.clone();
                self.spawn(out, async move {
                    let result = call(client.add_note(AddNoteRequest {
                        target: Some(note_target(message_id, thread)),
                        body_md: body,
                        author: NoteAuthor::User as i32,
                        // Empty: a keystroke in the TUI is issued once, so there
                        // is nothing to deduplicate — the same reasoning every
                        // other idempotency key here follows.
                        idempotency_key: String::new(),
                    }))
                    .await;
                    match result {
                        Ok(response) => Msg::Done {
                            label: format!("note {} added", response.into_inner().id),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "note add".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::NoteList {
                generation,
                message_id,
                thread,
            } => {
                let mut client = self.notes.clone();
                self.report(generation, out, async move {
                    call(client.list_notes(ListNotesRequest {
                        target: Some(note_target(message_id, thread)),
                    }))
                    .await
                    .map(|r| wire::note_rows(&r.into_inner()))
                });
            }
            Cmd::NoteWatch {
                generation,
                message_id,
                thread,
            } => {
                let mut client = self.notes.clone();
                self.stream_report(generation, out, move |sink| async move {
                    watch_notes(
                        client.watch_notes(WatchNotesRequest {
                            target: Some(note_target(message_id, thread)),
                        }),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::NoteEdit { note_id, body } => {
                let mut client = self.notes.clone();
                self.spawn(out, async move {
                    match call(client.edit_note(EditNoteRequest {
                        note_id,
                        body_md: body,
                    }))
                    .await
                    {
                        Ok(_) => Msg::Done {
                            label: format!("note {note_id} rewritten"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "note edit".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::NoteDelete { note_id } => {
                let mut client = self.notes.clone();
                self.spawn(out, async move {
                    match call(client.delete_note(DeleteNoteRequest { note_id })).await {
                        Ok(_) => Msg::Done {
                            label: format!("note {note_id} deleted"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "note rm".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::SavedList {
                generation,
                account_id,
            } => {
                let mut client = self.saved.clone();
                self.report(generation, out, async move {
                    call(client.list_saved_searches(ListSavedSearchesRequest { account_id }))
                        .await
                        .map(|r| wire::saved_rows(&r.into_inner()))
                });
            }
            Cmd::SavedSet {
                account_id,
                name,
                query,
                update,
            } => {
                let mut client = self.saved.clone();
                self.spawn(out, async move {
                    let result = if update {
                        call(client.update_saved_search(UpdateSavedSearchRequest {
                            account_id,
                            name,
                            query,
                        }))
                        .await
                    } else {
                        call(client.create_saved_search(CreateSavedSearchRequest {
                            account_id,
                            name,
                            query,
                        }))
                        .await
                    };
                    match result {
                        Ok(response) => Msg::Done {
                            label: wire::saved_stored(&response.into_inner()),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: if update { "saved edit" } else { "saved save" }.to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::SavedRun {
                generation,
                account_id,
                name,
                limit,
                explain,
            } => {
                let mut client = self.saved.clone();
                self.stream_report(generation, out, move |sink| async move {
                    stream_saved_hits(
                        client.run_saved_search(RunSavedSearchRequest {
                            account_id,
                            name,
                            limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                            explain,
                        }),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::SavedDelete { account_id, name } => {
                let mut client = self.saved.clone();
                let label = name.clone();
                self.spawn(out, async move {
                    match call(
                        client.delete_saved_search(DeleteSavedSearchRequest { account_id, name }),
                    )
                    .await
                    {
                        Ok(_) => Msg::Done {
                            label: format!("{label} forgotten"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "saved rm".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::FolderList {
                generation,
                account_id,
            } => {
                let mut client = self.saved.clone();
                self.report(generation, out, async move {
                    call(client.list_smart_folders(ListSmartFoldersRequest { account_id }))
                        .await
                        .map(|r| wire::smart_folder_rows(&r.into_inner()))
                });
            }
            Cmd::FolderCreate {
                generation,
                account_id,
                name,
                text,
                compile,
                auto_tag,
                notify,
                refresh,
            } => {
                let mut client = self.saved.clone();
                self.report(generation, out, async move {
                    if compile {
                        let compiled =
                            call(client.compile_smart_folder(CompileSmartFolderRequest {
                                account_id,
                                name,
                                description: text,
                                auto_tag: auto_tag.unwrap_or_default(),
                                notify,
                                refresh,
                            }))
                            .await?
                            .into_inner();
                        return Ok(compiled.folder.as_ref().map_or_else(Vec::new, |folder| {
                            wire::smart_folder_fields(folder, compiled.plan.as_ref())
                        }));
                    }
                    call(client.create_smart_folder(CreateSmartFolderRequest {
                        account_id,
                        name,
                        predicate: text,
                        auto_tag: auto_tag.unwrap_or_default(),
                        notify,
                    }))
                    .await
                    .map(|r| wire::smart_folder_fields(&r.into_inner(), None))
                });
            }
            Cmd::FolderMembers {
                generation,
                account_id,
                name,
                limit,
            } => {
                let mut client = self.saved.clone();
                self.stream_report(generation, out, move |sink| async move {
                    stream_members(
                        client.list_smart_folder_members(ListSmartFolderMembersRequest {
                            account_id,
                            name,
                            limit: u32::try_from(limit.unwrap_or(0)).unwrap_or(u32::MAX),
                        }),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::FolderEval {
                generation,
                account_id,
                name,
            } => {
                let mut client = self.saved.clone();
                self.report(generation, out, async move {
                    call(
                        client
                            .evaluate_smart_folder(EvaluateSmartFolderRequest { account_id, name }),
                    )
                    .await
                    .map(|r| wire::evaluation_rows(&r.into_inner()))
                });
            }
            Cmd::FolderDelete { account_id, name } => {
                let mut client = self.saved.clone();
                let label = name.clone();
                self.spawn(out, async move {
                    match call(
                        client.delete_smart_folder(DeleteSmartFolderRequest { account_id, name }),
                    )
                    .await
                    {
                        Ok(_) => Msg::Done {
                            label: format!("{label} forgotten"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "folder rm".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }

            // -- automation and notifications (task 98) -----------------------
            Cmd::WebhookList {
                generation,
                reveal_url,
            } => {
                let mut client = self.webhooks.clone();
                self.report(generation, out, async move {
                    call(client.list(ListWebhooksRequest { reveal_url }))
                        .await
                        .map(|r| wire::destination_rows(&r.into_inner().destinations))
                });
            }
            Cmd::WebhookAdd {
                generation,
                name,
                url,
                template,
                events,
                include_body,
                disabled,
                secret,
                max_attempts,
            } => {
                let mut client = self.webhooks.clone();
                // A report rather than a fact: `Register` echoes the destination
                // back, and what it echoes is the answer — the URL as stored,
                // the events actually subscribed to, whether it is entitled to
                // bodies. A one-line "registered" would hide all of it.
                self.report(generation, out, async move {
                    let (secret_source, secret_reference) = webhook_secret(secret.as_ref());
                    let request = RegisterWebhookRequest {
                        name,
                        url,
                        template: match template {
                            commands::automation::Template::Generic => WebhookTemplate::Generic,
                            commands::automation::Template::Slack => WebhookTemplate::Slack,
                        } as i32,
                        events: events
                            .iter()
                            .filter_map(|name| wire::webhook_event(name))
                            .map(|event| event as i32)
                            .collect(),
                        include_body,
                        disabled,
                        secret_source: secret_source as i32,
                        secret_reference,
                        max_attempts: max_attempts.unwrap_or(0),
                    };
                    call(client.register(request)).await.map(|r| {
                        r.into_inner()
                            .destination
                            .as_ref()
                            .map_or_else(Vec::new, |destination| {
                                vec![wire::destination_row(destination)]
                            })
                    })
                });
            }
            Cmd::WebhookRemove { name } => {
                let mut client = self.webhooks.clone();
                let label = name.clone();
                self.spawn(out, async move {
                    match call(client.remove(RemoveWebhookRequest { name })).await {
                        // Removal is idempotent rather than an error, so the
                        // answer distinguishes "gone now" from "was not there" —
                        // a script converging on "gone" wants both to succeed,
                        // and a person wants to know which happened.
                        Ok(response) => Msg::Done {
                            label: if response.into_inner().removed {
                                format!("{label} removed, with its delivery history")
                            } else {
                                format!("no destination named {label}")
                            },
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "webhook rm".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::WebhookEnabled {
                generation,
                name,
                enabled,
            } => {
                let mut client = self.webhooks.clone();
                self.report(generation, out, async move {
                    call(client.set_enabled(SetWebhookEnabledRequest { name, enabled }))
                        .await
                        .map(|r| {
                            r.into_inner()
                                .destination
                                .as_ref()
                                .map_or_else(Vec::new, |destination| {
                                    vec![wire::destination_row(destination)]
                                })
                        })
                });
            }
            Cmd::WebhookDeliveries {
                generation,
                destination,
                limit,
                show_payload,
            } => {
                let mut client = self.webhooks.clone();
                self.report(generation, out, async move {
                    call(client.list_deliveries(ListDeliveriesRequest {
                        destination: destination.unwrap_or_default(),
                        limit: limit.unwrap_or(0),
                        include_payload: show_payload,
                    }))
                    .await
                    .map(|r| wire::delivery_rows(&r.into_inner().deliveries))
                });
            }
            Cmd::WebhookReplay {
                generation,
                delivery_id,
            } => {
                let mut client = self.webhooks.clone();
                self.report(generation, out, async move {
                    call(client.replay_delivery(ReplayDeliveryRequest { delivery_id }))
                        .await
                        .map(|r| {
                            r.into_inner()
                                .delivery
                                .as_ref()
                                .map_or_else(Vec::new, |delivery| {
                                    vec![wire::delivery_row(delivery)]
                                })
                        })
                });
            }
            Cmd::Forward {
                generation,
                message_id,
                destination,
            } => {
                let mut client = self.webhooks.clone();
                let reporter = out.clone();
                self.report(generation, out, async move {
                    let response = call(client.forward(ForwardMessageRequest {
                        message_id,
                        destination,
                    }))
                    .await?
                    .into_inner();
                    // The status line says "queued", never "sent", and says so
                    // louder when no dispatcher is running — see
                    // `wire::forwarded`. Sent alongside the report because the
                    // row shows the queue entry and the line shows what that
                    // means.
                    let _ = reporter.send(Msg::Done {
                        label: wire::forwarded(&response),
                        result: Ok(Effect::None),
                    });
                    Ok(response
                        .delivery
                        .as_ref()
                        .map_or_else(Vec::new, |delivery| vec![wire::delivery_row(delivery)]))
                });
            }
            Cmd::HookList { generation } => {
                let mut client = self.hooks.clone();
                self.report(generation, out, async move {
                    call(client.list_hooks(ListHooksRequest {}))
                        .await
                        .map(|r| wire::hook_rows(&r.into_inner()))
                });
            }
            Cmd::HookTest {
                generation,
                name,
                event_json,
            } => {
                let mut client = self.hooks.clone();
                self.report(generation, out, async move {
                    call(client.test_hook(TestHookRequest { name, event_json }))
                        .await
                        .map(|r| wire::hook_test_rows(&r.into_inner()))
                });
            }
            Cmd::NotifyAlerts {
                generation,
                since_id,
            } => {
                let mut client = self.notify.clone();
                self.stream_report(generation, out, move |sink| async move {
                    stream_alerts(client.stream_alerts(StreamAlertsRequest { since_id }), sink)
                        .await;
                });
            }
            Cmd::NotifyScore {
                generation,
                message_id,
            } => {
                let mut client = self.notify.clone();
                self.report(generation, out, async move {
                    call(client.score_message(ScoreMessageRequest { message_id }))
                        .await
                        .map(|r| wire::score_rows(&r.into_inner()))
                });
            }

            // -- accounts and tokens (task 97) --------------------------------
            Cmd::AccountList { generation, open } => {
                let mut client = self.accounts.clone();
                self.report(generation, out, async move {
                    call(client.list(ListAccountsRequest {}))
                        .await
                        .map(|r| wire::account_rows(&r.into_inner(), open))
                });
            }
            Cmd::AccountShow {
                generation,
                account_id,
            } => {
                let mut client = self.accounts.clone();
                self.report(generation, out, async move {
                    call(client.get(GetAccountRequest { id: account_id }))
                        .await
                        .map(|r| wire::account_fields(&r.into_inner()))
                });
            }
            Cmd::AccountDiscover {
                generation,
                email,
                credential,
                allow_model,
            } => {
                let mut client = self.accounts.clone();
                let reporter = out.clone();
                self.report(generation, out, async move {
                    let response = call(client.autoconfigure(AutoconfigureRequest {
                        email: email.clone(),
                        credential: credential.as_ref().map(credential_ref),
                        allow_model_fallback: allow_model,
                    }))
                    .await?
                    .into_inner();
                    // The block travels back to the model as well as into the
                    // report, for the reason a rule draft does: `:account toml`
                    // opens it after the report has been closed, and a document
                    // that only ever existed inside a rendered row could not be
                    // opened at all.
                    if !response.toml.is_empty() {
                        let _ = reporter.send(Msg::Block(Box::new(ConfigBlock::new(
                            "the [[accounts]] block",
                            response.toml.clone(),
                            rmail_core::config_path_from_env(),
                            // Accounts are the one block with a wire alternative,
                            // and the row says so rather than leaving a reader to
                            // guess that `:account new` exists.
                            ReadOnlyReason::AlsoOverTheWire("account new"),
                            "rmaild picks it up on its next restart",
                        ))));
                    }
                    Ok(wire::autoconfigure_rows(&email, &response))
                });
            }
            Cmd::AccountCreate { name, settings } => {
                let mut client = self.accounts.clone();
                self.spawn(out, async move {
                    let request = new_account(name, &settings);
                    match call(client.create(request)).await {
                        Ok(response) => Msg::Done {
                            label: wire::account_created(&response.into_inner()),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "account new".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::AccountTest {
                generation,
                account_id,
            } => {
                let mut client = self.accounts.clone();
                self.report(generation, out, async move {
                    call(client.test_connection(TestConnectionRequest { id: account_id }))
                        .await
                        .map(|r| wire::account_test_rows(&r.into_inner()))
                });
            }
            Cmd::AccountDelete { account_id } => {
                let mut client = self.accounts.clone();
                self.spawn(out, async move {
                    match call(client.delete(DeleteAccountRequest { id: account_id })).await {
                        Ok(_) => Msg::Done {
                            label: format!("account {account_id} deleted"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "account rm".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::AccountLogin {
                generation,
                account_id,
                provider,
                client_id,
                client_secret_command,
                scopes,
                open_browser,
            } => {
                let mut client = self.accounts.clone();
                let opener = self.opener.clone();
                self.stream_report(generation, out, move |sink| async move {
                    oauth_login(
                        &mut client,
                        BeginOAuthRequest {
                            account_id,
                            provider,
                            client_id,
                            client_secret_command,
                            scopes,
                        },
                        open_browser.then_some(opener),
                        sink,
                    )
                    .await;
                });
            }
            Cmd::AccountRefresh {
                generation,
                account_id,
                force,
            } => {
                let mut client = self.accounts.clone();
                self.report(generation, out, async move {
                    call(client.refresh_token(RefreshTokenRequest { account_id, force }))
                        .await
                        .map(|r| wire::refresh_rows(&r.into_inner()))
                });
            }
            Cmd::TokenList { generation } => {
                let mut client = self.admin.clone();
                self.report(generation, out, async move {
                    call(client.list_tokens(ListTokensRequest {}))
                        .await
                        .map(|r| wire::token_rows(&r.into_inner()))
                });
            }
            Cmd::TokenCreate {
                generation,
                name,
                scopes,
                ttl_secs,
            } => {
                let mut client = self.admin.clone();
                self.report(generation, out, async move {
                    call(client.mint_token(MintTokenRequest {
                        name,
                        scopes,
                        ttl_secs,
                    }))
                    .await
                    .map(|r| wire::minted_rows(&r.into_inner()))
                });
            }
            Cmd::TokenRevoke { token_id } => {
                let mut client = self.admin.clone();
                self.spawn(out, async move {
                    match call(client.revoke_token(RevokeTokenRequest { id: token_id })).await {
                        Ok(_) => Msg::Done {
                            label: format!("token {token_id} revoked"),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "token revoke".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::OpenText {
                text,
                extension,
                label,
            } => {
                let opener = self.opener.clone();
                self.spawn(out, async move {
                    let result = tokio::task::spawn_blocking(move || {
                        html::open_text(&extension, &text, &opener)
                            .map_err(|error| format!("{error:#}"))
                    })
                    .await
                    .unwrap_or_else(|error| Err(format!("opening: {error}")));
                    match result {
                        // The path, because the handler the platform picked may
                        // not be one the reader expected — and a file they can
                        // find is a file they can open by hand.
                        Ok(path) => Msg::Done {
                            label: format!("{label} — {}", path.display()),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: label.clone(),
                            result: Err(error),
                        },
                    }
                });
            }

            // -- AI policy, safety and audit (task 96) ------------------------
            Cmd::BudgetStatus {
                generation,
                account_id,
            } => {
                let mut client = self.policy.clone();
                self.report(generation, out, async move {
                    call(client.get_spend(GetSpendRequest { account_id }))
                        .await
                        .map(|r| wire::budget_rows(&r.into_inner()))
                });
            }
            Cmd::BudgetForm {
                generation,
                account_id,
                class,
            } => {
                let mut client = self.policy.clone();
                // Through the reporting slot, like every other read that fills a
                // pane: only one of the two panes is on screen at a time, so a
                // second request always supersedes the first and `Esc` has one
                // thing to abort.
                self.spawn_superseding(&self.reporting, async move {
                    let event = match call(client.get_spend(GetSpendRequest { account_id })).await {
                        Ok(response) => FormEvent::Fields(wire::budget_fields(
                            &response.into_inner(),
                            class == commands::ai_policy::Class::Bulk,
                        )),
                        Err(error) => FormEvent::Failed(error),
                    };
                    let _ = out.send(Msg::Form { generation, event });
                });
            }
            Cmd::BudgetSet {
                account_id,
                class,
                caps,
            } => {
                let mut client = self.policy.clone();
                self.spawn(out, async move {
                    let request = SetBudgetRequest {
                        account_id,
                        class: match class {
                            commands::ai_policy::Class::All => BudgetClass::All,
                            commands::ai_policy::Class::Bulk => BudgetClass::Bulk,
                        } as i32,
                        caps: Some(budget_caps(&caps)),
                    };
                    match call(client.set_budget(request)).await {
                        // What was *stored*, echoed from the response rather
                        // than repeated from the request: this RPC replaces a
                        // whole budget, and the one thing a caller needs
                        // confirmed is which caps are now in force.
                        Ok(response) => Msg::Done {
                            label: wire::budget_stored(&response.into_inner()),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "ai budget set".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::ProviderStatus {
                generation,
                account_id,
            } => {
                let mut client = self.policy.clone();
                self.report(generation, out, async move {
                    call(client.get_ai_provider(GetAiProviderRequest { account_id }))
                        .await
                        .map(|r| wire::provider_rows(&r.into_inner()))
                });
            }
            Cmd::ProviderSet {
                account_id,
                provider,
            } => {
                let mut client = self.policy.clone();
                self.spawn(out, async move {
                    let request = SetAiProviderRequest {
                        account_id,
                        provider: match provider {
                            commands::ai_policy::Provider::Claude => AiProviderKind::Claude,
                            commands::ai_policy::Provider::Local => AiProviderKind::Local,
                            // UNSPECIFIED *clears* the override on a set, which
                            // is what `clear`/`inherit` asked for — see that
                            // enum's own docs on why absence is spelled rather
                            // than implied.
                            commands::ai_policy::Provider::Inherit => AiProviderKind::Unspecified,
                        } as i32,
                    };
                    match call(client.set_ai_provider(request)).await {
                        Ok(response) => Msg::Done {
                            label: wire::provider_set(&response.into_inner()),
                            result: Ok(Effect::None),
                        },
                        Err(error) => Msg::Done {
                            label: "ai provider set".to_owned(),
                            result: Err(error),
                        },
                    }
                });
            }
            Cmd::ScanInjection {
                generation,
                message_id,
            } => {
                let mut client = self.safety.clone();
                self.report(generation, out, async move {
                    call(client.scan_injection(ScanInjectionRequest { message_id }))
                        .await
                        .map(|r| wire::injection_rows(&r.into_inner()))
                });
            }
            Cmd::ConfirmInjection {
                generation,
                message_id,
                confirm,
            } => {
                let mut client = self.safety.clone();
                self.report(generation, out, async move {
                    // Scanned first, even to confirm, exactly as
                    // `mail ai scan-injection --confirm` does: a confirmation is
                    // consent to a *specific* set of findings — the daemon clears
                    // it when a re-scan turns up different ones — so confirming
                    // without having just seen them would be consent to whatever
                    // a stale row happened to hold.
                    let scan = call(client.scan_injection(ScanInjectionRequest { message_id }))
                        .await?
                        .into_inner();
                    if !scan.flagged {
                        // Not an error: the message is already in the state that
                        // was asked for. Reporting the scan says so, and says it
                        // in the same shape a scan does.
                        return Ok(wire::injection_rows(&scan));
                    }
                    let response = call(client.confirm_injection(ConfirmInjectionRequest {
                        message_id,
                        confirmed: confirm.confirmed(),
                    }))
                    .await?
                    .into_inner();
                    Ok(response
                        .flag
                        .as_ref()
                        .map_or_else(Vec::new, wire::injection_rows))
                });
            }
            Cmd::AuditQuery {
                generation,
                account_id,
                model,
                failed_only,
                whole_ledger,
            } => {
                let filter = AuditFilter {
                    // Zero is "every account" for this filter, so it is sent as
                    // an absent field rather than as a literal 0 — which the
                    // ledger would read as "the account whose id is 0" and match
                    // nothing.
                    account_id: (account_id != 0).then_some(account_id),
                    model,
                    status: failed_only.then_some(CallStatus::Error as i32),
                    ..AuditFilter::default()
                };
                if whole_ledger {
                    let mut client = self.audit.clone();
                    self.stream_report(generation, out, move |sink| async move {
                        export_ledger(
                            client.export_ledger(ExportLedgerRequest {
                                filter: Some(filter),
                            }),
                            sink,
                        )
                        .await;
                    });
                } else {
                    let mut client = self.audit.clone();
                    self.report(generation, out, async move {
                        call(client.query_ai_calls(QueryAiCallsRequest {
                            filter: Some(filter),
                            // The server clamps this, and the pane caps what it
                            // keeps at `overlays::MAX_ROWS` anyway; asking for
                            // exactly that many is what makes "one page" and
                            // "one screenful" the same thing here.
                            limit: i32::try_from(super::overlays::MAX_ROWS).unwrap_or(i32::MAX),
                            before_id: None,
                        }))
                        .await
                        .map(|r| wire::audit_rows(&r.into_inner().entries))
                    });
                }
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

    /// One frame of rows that *extend* what is already shown, for a stream that
    /// sends each row once — `SuggestTags`' discipline rather than the snapshot
    /// one above.
    fn append(&self, rows: Vec<ReportRow>, complete: bool) {
        let _ = self.out.send(Msg::Report {
            generation: self.generation,
            event: ReportEvent::Frame {
                fill: ReportFill::Append,
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

/// The `TagSyncMode` a `--sync` value means.
const fn tag_sync_mode(sync: commands::tag::Sync) -> TagSyncMode {
    match sync {
        commands::tag::Sync::Local => TagSyncMode::Local,
        commands::tag::Sync::Imap => TagSyncMode::Imap,
        commands::tag::Sync::Auto => TagSyncMode::Auto,
    }
}

/// Apply or remove one tag across a selection, a row per message.
///
/// Sequential rather than concurrent, and deliberately: these reflect to IMAP,
/// and fanning a fifty-message selection out into fifty simultaneous STORE
/// commands is the "500 concurrent IMAP mutations from one keystroke" that
/// `model::MAX_BULK` exists to prevent. The report fills in as each lands, which
/// is also what makes progress visible on a slow server.
///
/// Every frame carries every row so far, because `ReportFill::Replace` is the
/// snapshot discipline the streamed reports share — see
/// `wire::index_progress_rows`.
async fn apply_tags(
    client: &mut TagServiceClient<Conn>,
    message_ids: &[i64],
    name: &str,
    remove: bool,
    sink: &ReportSink,
) {
    if message_ids.is_empty() {
        return sink.rows(Vec::new(), true);
    }
    let mut rows: Vec<ReportRow> = Vec::new();
    for (index, message_id) in message_ids.iter().enumerate() {
        let target = Some(Target {
            of: Some(target::Of::MessageId(*message_id)),
        });
        let names = vec![name.to_owned()];
        let outcome = if remove {
            call(client.remove_tag(RemoveTagRequest { target, names }))
                .await
                .map(|_| "removed".to_owned())
        } else {
            call(client.add_tag(AddTagRequest { target, names }))
                .await
                .map(|response| {
                    response
                        .into_inner()
                        .applications
                        .first()
                        .map_or_else(|| "applied".to_owned(), |a| wire::tag_source(a.source))
                })
        };
        rows.push(match outcome {
            Ok(what) => wire::tag_applied_row(*message_id, name, &what),
            // Kept going rather than abandoned: a tag that failed on one message
            // is a fact about that message, and stopping would leave the rest
            // untagged for no reason the reader could see.
            Err(error) => wire::tag_failed_row(*message_id, name, &error),
        });
        sink.rows(rows.clone(), index + 1 == message_ids.len());
    }
}

/// Run the whole OAuth flow, reporting each half as it lands.
///
/// Two RPCs and one report, because they are two halves of one act: `BeginOAuth`
/// binds a loopback port and returns the URL, `CompleteOAuth` blocks until the
/// browser comes back. A client that issued only the first would leave a port
/// held for a flow nobody could finish.
///
/// The URL is reported *before* the second call, not after: the second one blocks
/// until a human has consented, and a report that showed nothing until then would
/// be a report withholding the one thing the human needs to act on.
///
/// The browser is launched after that frame is sent, for the same reason. A
/// launch that fails is reported and the flow continues — the URL is on screen,
/// which is the whole point of drawing it.
async fn oauth_login(
    client: &mut AccountServiceClient<Conn>,
    request: BeginOAuthRequest,
    opener: Option<CommandOpener>,
    sink: ReportSink,
) {
    let started = match call(client.begin_o_auth(request)).await {
        Ok(response) => response.into_inner(),
        Err(error) => return sink.failed(error),
    };
    sink.rows(wire::oauth_started_rows(&started), false);
    if let Some(opener) = opener {
        let url = started.authorization_url.clone();
        let launched = tokio::task::spawn_blocking(move || html::open_url(&url, &opener)).await;
        if !matches!(launched, Ok(Ok(()))) {
            sink.rows(
                [
                    wire::oauth_started_rows(&started),
                    vec![wire::oauth_no_browser_row()],
                ]
                .concat(),
                false,
            );
        }
    }
    // No client-side deadline: this call is *supposed* to block while a human
    // reads a consent screen, and `RPC_TIMEOUT` would abandon the flow while
    // they were still deciding. The daemon releases the port at
    // `started.expires_at`, which the report says, and `Esc` aborts the task.
    match client
        .complete_o_auth(CompleteOAuthRequest {
            flow_id: started.flow_id.clone(),
        })
        .await
    {
        Ok(response) => sink.rows(wire::oauth_done_rows(&response.into_inner()), true),
        Err(status) => sink.failed(status.message().to_owned()),
    }
}

/// One credential *reference* on the wire — how to obtain the password, never
/// the password.
fn credential_ref(credential: &Credential) -> CredentialRef {
    use rmail_proto::v1::credential_ref::Source;
    CredentialRef {
        source: Some(match credential {
            Credential::Command(value) => Source::PasswordCommand(value.clone()),
            Credential::Env(value) => Source::PasswordEnv(value.clone()),
            Credential::Keychain(value) => Source::Keychain(value.clone()),
            Credential::OAuth(value) => Source::Oauth(value.clone()),
        }),
    }
}

/// A `:account new` line's `(flag, value)` pairs as the RPC's request.
///
/// A value that does not parse is *dropped*, which leaves that setting unset.
/// Reachable only from a line `commands::account::settings` already checked, so
/// this is the belt to that braces — and dropping is the safe direction: a port
/// that arrived as `0` would be stored as a port nothing can connect to.
fn new_account(name: String, settings: &[(String, String)]) -> CreateAccountRequest {
    let text = |flag: &str| {
        settings
            .iter()
            .find(|(name, _)| name == flag)
            .map(|(_, value)| value.clone())
    };
    let port = |flag: &str| text(flag).and_then(|value| value.parse::<u32>().ok());
    let credential = [
        ("password-command", Credential::Command as fn(String) -> _),
        ("password-env", Credential::Env),
        ("keychain", Credential::Keychain),
        ("oauth", Credential::OAuth),
    ]
    .into_iter()
    .find_map(|(flag, wrap)| text(flag).map(wrap));
    CreateAccountRequest {
        name,
        imap_server: text("imap-server"),
        imap_port: port("imap-port"),
        username: text("username"),
        smtp_server: text("smtp-server"),
        smtp_port: port("smtp-port"),
        credential: credential.as_ref().map(credential_ref),
    }
}

/// A window's absolute bounds from a duration and an optional end.
///
/// The one place a clock is read for these reports. `update` is pure, so the
/// `Cmd` carries "this many seconds back" and the conversion happens here — see
/// `commands::content`'s module docs. Zero means "the daemon's own default", which
/// is what every one of these RPCs reads an absent bound as.
fn window(since_secs: Option<i64>, until: Option<i64>) -> (i64, i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let end = until.unwrap_or(now);
    let start = since_secs.map_or(0, |secs| end.saturating_sub(secs));
    (start, if until.is_some() { end } else { 0 })
}

/// A note's target on the wire — a message, or the thread it belongs to.
fn note_target(message_id: i64, thread: bool) -> NoteTarget {
    use rmail_proto::v1::note_target::Of;
    NoteTarget {
        of: Some(if thread {
            Of::ThreadId(message_id)
        } else {
            Of::MessageId(message_id)
        }),
    }
}

/// An export's framing on the wire.
fn export_format(format: commands::content::analytics::Format) -> ExportFormat {
    match format {
        commands::content::analytics::Format::Mbox => ExportFormat::Mbox,
        commands::content::analytics::Format::Maildir => ExportFormat::Maildir,
        commands::content::analytics::Format::Eml => ExportFormat::Eml,
        commands::content::analytics::Format::Json => ExportFormat::Json,
    }
}

/// The same framing as `rmail_core::export::Format`, which owns the writer.
fn archive_format(format: commands::content::analytics::Format) -> ArchiveFormat {
    match format {
        commands::content::analytics::Format::Mbox => ArchiveFormat::Mbox,
        commands::content::analytics::Format::Maildir => ArchiveFormat::Maildir,
        commands::content::analytics::Format::Eml => ArchiveFormat::Eml,
        commands::content::analytics::Format::Json => ArchiveFormat::Json,
    }
}

/// Run an export, writing the archive and reporting what landed.
///
/// The writer is `rmail_core::export::write::DestinationWriter` — the same shared
/// code `mail export` uses, which owns the check keeping a server-supplied entry
/// name inside the directory the caller named. A second writer here would be a
/// second place that check could be got wrong.
///
/// One blocking task fed by a bounded channel rather than one `spawn_blocking`
/// per frame, for the reason `export_cli` gives: the channel's bound is what gives
/// the gRPC stream backpressure when the disk is slower than the socket.
///
/// A partial archive is left on disk in every failure path. Deleting a
/// half-written export would destroy the only copy of whatever did arrive; saying
/// it is partial is what stops it being mistaken for a whole one.
async fn run_export(
    client: &mut ExportServiceClient<Conn>,
    request: ExportRequest,
    format: commands::content::analytics::Format,
    to: PathBuf,
    sink: ReportSink,
) {
    let format = archive_format(format);
    let mut stream = match client.export(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ExportPart>(EXPORT_QUEUE);
    let destination = to.clone();
    let writer = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut writer = DestinationWriter::create(format, &destination)
            .map_err(|error| format!("creating {}: {error}", destination.display()))?;
        while let Some(part) = rx.blocking_recv() {
            writer.apply(&part).map_err(|error| error.to_string())?;
        }
        writer.finish().map_err(|error| error.to_string())
    });

    let mut messages = 0_u64;
    let mut done = None;
    let mut failed = None;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(status) => {
                failed = Some(status.message().to_owned());
                break;
            }
        };
        if let Some(summary) = chunk.done {
            done = Some(summary);
            break;
        }
        if chunk.start_of_message {
            messages += 1;
            // Progress, replacing each time: an export of forty thousand
            // messages is the case this pane exists for, and a row per message
            // would fill it long before the archive was written.
            sink.rows(
                vec![ReportRow::new([
                    "writing".to_owned(),
                    format!("{messages} message(s) so far → {}", to.display()),
                ])],
                false,
            );
        }
        if tx.send(part_from_proto(chunk)).await.is_err() {
            // The writer died; its error is the real one, so stop feeding it and
            // let the join report why.
            break;
        }
    }
    // Closing the channel is what ends the writer's loop, and it has to happen
    // before the join or this deadlocks.
    drop(tx);
    match writer.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return sink.failed(error),
        Err(error) => return sink.failed(format!("the export writer stopped: {error}")),
    }
    if let Some(error) = failed {
        return sink.failed(format!(
            "after {messages} message(s) — {} is incomplete: {error}",
            to.display()
        ));
    }
    // A gRPC stream that stops yielding ends OK, so without this a daemon that
    // shut down mid-export would leave a truncated archive reported as a whole
    // one.
    let Some(done) = done else {
        return sink.failed(format!(
            "the stream ended with no completion marker after {messages} message(s) — {} is \
             incomplete",
            to.display()
        ));
    };
    sink.rows(wire::export_rows(&to.display().to_string(), &done), true);
}

/// One export frame as the shared writer's own chunk type.
fn part_from_proto(chunk: ExportChunk) -> ExportPart {
    ExportPart {
        path: (!chunk.path.is_empty()).then_some(chunk.path),
        start_of_message: chunk.start_of_message,
        message_id: (chunk.message_id != 0).then_some(chunk.message_id),
        data: chunk.data,
    }
}

/// Drain an `AskAttachment` stream into a Report.
///
/// The prose is accumulated into rows as it arrives and the citations follow it,
/// which is the order the proto fixes: an inline `[n]` is only resolvable once the
/// whole answer has been seen. A refusal is reported as a failure, because an
/// ungrounded answer this daemon declined to give is not an answer.
async fn ask_attachment<S>(request: S, sink: ReportSink)
where
    S: Future<
        Output = Result<tonic::Response<tonic::Streaming<AskAttachmentChunk>>, tonic::Status>,
    >,
{
    use rmail_proto::v1::ask_attachment_chunk::Body;
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    let mut rows: Vec<ReportRow> = Vec::new();
    let mut prose = String::new();
    loop {
        let Some(item) = stream.next().await else {
            return sink.failed("the daemon ended the answer early".to_owned());
        };
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(status) => return sink.failed(status.message().to_owned()),
        };
        match chunk.body {
            Some(Body::Trace(trace)) => {
                rows.push(
                    ReportRow::new([
                        "read".to_owned(),
                        format!(
                            "{} passage(s) from {} attachment(s){}",
                            trace.passages,
                            trace.attachments,
                            if trace.withheld_by_policy > 0 {
                                format!(", {} withheld by policy", trace.withheld_by_policy)
                            } else {
                                String::new()
                            }
                        ),
                    ])
                    .toned(if trace.withheld_by_policy > 0 {
                        ReportTone::Warn
                    } else {
                        ReportTone::Muted
                    }),
                );
                sink.rows(rows.clone(), false);
            }
            Some(Body::Token(token)) => {
                prose.push_str(&token);
                let mut frame = rows.clone();
                for line in prose.lines() {
                    frame.push(ReportRow::new([String::new(), line.to_owned()]));
                }
                sink.rows(frame, false);
            }
            Some(Body::Citation(citation)) => {
                rows.push(ReportRow::new([
                    format!("[{}]", citation.label),
                    format!("{} — {}", citation.filename, citation.quote),
                ]));
            }
            Some(Body::Usage(_)) => {}
            Some(Body::Done(done)) => {
                if !done.refusal.is_empty() {
                    return sink.failed(done.refusal);
                }
                let mut frame = rows.clone();
                for line in prose.lines() {
                    frame.push(ReportRow::new([String::new(), line.to_owned()]));
                }
                if !done.grounded {
                    frame.push(
                        ReportRow::new([
                            "ungrounded".to_owned(),
                            "this answer cites nothing — treat it as a guess".to_owned(),
                        ])
                        .toned(ReportTone::Warn),
                    );
                }
                return sink.rows(frame, true);
            }
            None => {}
        }
    }
}

/// Drain a `WatchNotes` stream into a Report.
///
/// Appends and never completes, like the alert feed: it is a live view, and a
/// border saying "done" would describe a stream that is still listening.
async fn watch_notes<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<WireNoteEvent>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(event)) => {
                if let Some(row) = wire::note_event_row(&event) {
                    sink.append(vec![row], false);
                }
            }
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.failed("the daemon closed the note stream".to_owned()),
        }
    }
}

/// Drain a `RunSavedSearch` stream into a Report.
///
/// Appends: hits arrive once, in rank order — `SearchService.Search`'s discipline,
/// which this RPC shares because it *is* that search under a stored name.
async fn stream_saved_hits<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<SearchHit>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(hit)) => sink.append(vec![wire::saved_hit_row(&hit)], false),
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.append(Vec::new(), true),
        }
    }
}

/// Drain a `ListSmartFolderMembers` stream into a Report.
async fn stream_members<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<ProtoMessage>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(message)) => sink.append(vec![wire::member_row(&message)], false),
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.append(Vec::new(), true),
        }
    }
}

/// Drain a `StreamAlerts` stream into a Report.
///
/// Appends, like `SuggestTags`: each alert is sent once, and unlike every other
/// streaming report here this one has no end — it is the live tail. So it never
/// completes: the border keeps saying it is listening, which is the truth, and
/// `Esc` is what stops it (through the `reporting` slot, like every other
/// stream).
///
/// A stream that *does* end has been ended by the daemon, and that is reported as
/// a failure rather than as completion — a live feed that silently stopped would
/// leave somebody watching a pane that can no longer tell them anything.
async fn stream_alerts<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<Alert>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(alert)) => sink.append(vec![wire::alert_row(&alert)], false),
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.failed("the daemon closed the alert stream".to_owned()),
        }
    }
}

/// A webhook's signing-key source and reference on the wire — a reference, never
/// the key.
fn webhook_secret(secret: Option<&commands::automation::Secret>) -> (WebhookSecretSource, String) {
    match secret {
        None => (WebhookSecretSource::Unspecified, String::new()),
        Some(commands::automation::Secret::Env(value)) => (WebhookSecretSource::Env, value.clone()),
        Some(commands::automation::Secret::Command(value)) => {
            (WebhookSecretSource::Command, value.clone())
        }
        Some(commands::automation::Secret::Keychain(value)) => {
            (WebhookSecretSource::Keychain, value.clone())
        }
    }
}

/// Drain an `ExportLedger` stream into a Report.
///
/// Appends, like `SuggestTags`: the ledger sends each entry once, newest first.
/// The stream ending cleanly *is* the end of the export — the proto is explicit
/// that a truncated one terminates with `CANCELLED` rather than `OK`, so a `None`
/// here can be trusted to mean "that was all of it".
async fn export_ledger<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<AuditEntry>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(entry)) => sink.append(vec![wire::audit_row(&entry)], false),
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.append(Vec::new(), true),
        }
    }
}

/// The eight `(flag, value)` caps a `:ai budget set` line carried, as the RPC's
/// nested request.
///
/// A value that does not parse is *dropped*, which is uncapping that dimension.
/// Reachable only from a bang'd line, because the form refuses a non-number
/// where it was typed (`commands::ai_policy::caps`) and the parse there is the
/// same one — so this is the belt to that braces, and dropping is the safe
/// direction: an unparseable cap must never become a `0`, which on this RPC
/// forbids all spending rather than allowing it.
fn budget_caps(caps: &[(String, String)]) -> BudgetCaps {
    let usd = |flag: &str| {
        caps.iter()
            .find(|(name, _)| name == flag)
            .and_then(|(_, value)| value.parse::<f64>().ok())
    };
    let tokens = |flag: &str| {
        caps.iter()
            .find(|(name, _)| name == flag)
            .and_then(|(_, value)| value.parse::<i64>().ok())
    };
    BudgetCaps {
        daily: Some(BudgetWindowCaps {
            soft_usd: usd("daily-soft-usd"),
            hard_usd: usd("daily-hard-usd"),
            soft_tokens: tokens("daily-soft-tokens"),
            hard_tokens: tokens("daily-hard-tokens"),
        }),
        monthly: Some(BudgetWindowCaps {
            soft_usd: usd("monthly-soft-usd"),
            hard_usd: usd("monthly-hard-usd"),
            soft_tokens: tokens("monthly-soft-tokens"),
            hard_tokens: tokens("monthly-hard-tokens"),
        }),
    }
}

/// Drain a `SuggestTags` stream into a Report.
///
/// Appends rather than replaces, which is the one streamed report here that
/// does: `SuggestTags` sends each suggestion once, so this is
/// `SearchService.Search`'s discipline and not the finder's. The stream ending
/// without a failure *is* the end of the suggestions — there is no terminal
/// frame to wait for — so the last thing this does is complete the report.
async fn stream_suggestions<S>(request: S, sink: ReportSink)
where
    S: Future<Output = Result<tonic::Response<tonic::Streaming<TagSuggestion>>, tonic::Status>>,
{
    let mut stream = match request.await {
        Ok(response) => response.into_inner(),
        Err(status) => return sink.failed(status.message().to_owned()),
    };
    loop {
        match stream.next().await {
            Some(Ok(suggestion)) => {
                sink.append(vec![wire::tag_suggestion_row(&suggestion)], false);
            }
            Some(Err(status)) => return sink.failed(status.message().to_owned()),
            None => return sink.append(Vec::new(), true),
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
        self.spawn_superseding(&self.watching, async move {
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
            // Batched on the same ticker as `Msg::Changed`, not sent per
            // event — see `ledger.rs`'s own module doc ("where a `Delta`
            // actually comes from, and why it is batched") for why an
            // earlier, uncoalesced draft of this loop turned a backlog
            // replay into one full repaint per historical row.
            let mut deltas: Vec<ledger::SeqDelta> = Vec::new();
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        if !deltas.is_empty()
                            && out.send(Msg::LedgerDelta(std::mem::take(&mut deltas))).is_err()
                        {
                            return;
                        }
                        if dirty {
                            dirty = false;
                            if out.send(Msg::Changed).is_err() {
                                return;
                            }
                        }
                    }
                    next = stream.next() => match next {
                        Some(Ok(event)) => {
                            dirty = true;
                            if let Some(delta) = wire::ledger_delta(&event) {
                                deltas.push(ledger::SeqDelta {
                                    seq: event.seq,
                                    delta,
                                });
                                // Early flush: see `DELTA_BATCH`'s own doc.
                                // A backlog replay is the only realistic way
                                // to hit this: ordinary live traffic never
                                // gets remotely close inside one tick.
                                if deltas.len() >= DELTA_BATCH
                                    && out.send(Msg::LedgerDelta(std::mem::take(&mut deltas))).is_err()
                                {
                                    return;
                                }
                            } else {
                                // The subscription itself is filtered to
                                // `NewMail`/`FlagChanged`/`Moved`/`Deleted`
                                // (see `request` above), so in practice
                                // every `None` here is a genuine anomaly —
                                // a relevant kind missing the ids it needs,
                                // or a `FlagChanged` payload that failed to
                                // parse — not routine kind filtering. Costs
                                // nothing to record even though it goes
                                // nowhere today: this binary installs no
                                // `tracing` subscriber (see this function's
                                // own note on why `LiveUpdatesStopped`,
                                // not `tracing`, is what actually reaches a
                                // user below), so this is dormant until
                                // something wires one up, not a live
                                // diagnostic yet.
                                tracing::debug!(
                                    seq = event.seq,
                                    kind = ?event.kind(),
                                    mailbox_id = ?event.mailbox_id,
                                    message_id = ?event.message_id,
                                    "watch event did not decode to a ledger delta"
                                );
                            }
                        }
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
