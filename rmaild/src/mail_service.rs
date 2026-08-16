//! The `MailService` gRPC implementation.
//!
//! `List`/`Get`/`GetThread` and the mutations (`Move`/`Copy`/`SetFlags`/
//! `Delete`) are thin translations over [`rmail_core::mail::MailStore`] —
//! MailStore owns the local-mirror reads and the IMAP-reflection/event/
//! ordering logic (see its module docs for the "IMAP first, local mirror
//! second" contract and why `Move` drops the local row rather than
//! re-pointing it). This file's own design lives in its two streaming RPCs:
//!
//! # `List` is bounded, not (yet) truly streamed — and that is what lets it
//! paginate
//!
//! `List` fetches its whole (capped — see
//! [`rmail_core::mail::MAX_LIST_LIMIT`]) page from [`MailStore::list`] before
//! wrapping it in [`tokio_stream::iter`]. That is a streamed *response*, in
//! the gRPC-framing sense the client sees — but not a streamed *read*: the
//! daemon holds the whole page in memory before the first frame goes out.
//! Bounded at 500 rows, this is not a leak; it is a real difference from
//! `GetAttachment`/`WatchEvents`, which genuinely produce their frames
//! incrementally.
//!
//! It is also what makes the opaque page token deliverable. A server-streamed
//! response has no envelope to carry a `next_page_token` field, and gRPC has
//! no supported way to add a trailer on a successful stream — but the *initial
//! metadata* is written when the handler returns, and by then this handler
//! already knows its whole page and therefore its next token. So the token
//! rides in the response headers under
//! [`rmail_core::page::NEXT_PAGE_TOKEN_METADATA_KEY`]. A handler that
//! genuinely streamed its read could not do this, and would have needed a
//! `page_token` field on a wrapper message — i.e. a breaking proto change.
//! The absence of the header is definitive: it means this was the last page.
//!
//! # `GetAttachment`: chunked well under the frame cap
//!
//! [`ATTACHMENT_CHUNK_BYTES`] (256 KiB) is small relative to
//! `grpc.limits.max_message_bytes`'s 16 MiB default, so an attachment of any
//! size streams as a sequence of frames the transport never has to reject for
//! being oversized — see `attachment_larger_than_one_chunk_streams_correctly`
//! in the integration tests for proof an attachment spanning several chunks
//! reassembles byte-for-byte. This bounds *frame* size, not daemon memory: the
//! whole attachment is decoded into memory by [`MailStore::attachment_bytes`]
//! before chunking starts (roughly the raw RFC822 blob plus the decoded copy,
//! held for the life of the stream), and there is no concurrency cap on
//! `GetAttachment` today — many concurrent, slowly-read streams hold that much
//! each. Fine for interactive use; a limit worth adding before this is opened
//! to many unauthenticated-by-content-size callers.
//!
//! # `WatchEvents`: the same replay-then-follow contract as `SyncService`
//!
//! This mirrors `SyncApi::watch_events` exactly — subscribe to the live tail
//! *before* reading the durable backlog, replay the backlog, then follow the
//! tail discarding anything at or below the cursor the backlog reached, with
//! lag recovery re-reading from the log rather than failing the stream. See
//! `rmaild::sync_service`'s module docs for the full reasoning; it is not
//! reproduced here beyond what differs. The two implementations are
//! deliberately not shared: they are independent gRPC surfaces bound to
//! independent core services (`SyncEngine` vs `MailStore`), and the ~100
//! lines in common are exactly the kind of thing worth revisiting behind a
//! shared helper in `rmail_core::events` if a third consumer ever needs it —
//! not worth the coupling for two.
//!
//! # Cancellation and deadlines
//!
//! Both streaming RPCs drive their work from a spawned task feeding a bounded
//! channel, exactly like `SyncApi::watch_events`. A client that drops the
//! response stream — whether it disconnected, or its local deadline elapsed
//! and it cancelled the call — closes the channel, and the producer notices
//! on its next send and exits; nothing polls a stream nobody is reading. The
//! producer's cancellation token is a child of the daemon's shutdown token, so
//! a producer blocked on a full channel (a slow-but-still-connected client)
//! also unwinds when the daemon shuts down, rather than holding graceful
//! shutdown open indefinitely — see
//! `a_shutdown_closes_an_open_attachment_stream_rather_than_holding_it` in
//! the integration tests.
//!
//! This project has no server-side deadline-enforcement layer today (no
//! `Timeout` `tower` layer is installed in `rmaild::serve_uds_with_engine`),
//! so "honoring the request deadline" and "honoring cancellation" are the
//! same mechanism from this file's side: a deadline that elapses is enforced
//! *client-side*, which cancels the call, which the server observes as the
//! response stream closing — precisely the path already covered above.
//!
//! The four unary mutations (`Move`/`Copy`/`SetFlags`/`Delete`) do *not*
//! thread the daemon's shutdown token into their IMAP call, unlike the two
//! streams above — matching `AccountApi::test_connection`'s existing
//! precedent (also an IMAP round trip behind a unary RPC, also not wired to
//! shutdown). This is a bounded gap, not an unbounded one: every command
//! `rmail_core::imap::mutate` issues is itself capped by
//! `rmail_core::imap::IMAP_DEADLINE` (30s) — but the cap is per *command*, not
//! per RPC, and a mutation is several commands. `Move` on the fallback path
//! (no `MOVE` capability) is handshake, SELECT, COPY, STORE, EXPUNGE and
//! LOGOUT, so the true bound on graceful shutdown is a low multiple of
//! `IMAP_DEADLINE` — up to roughly six times it — not `IMAP_DEADLINE` itself.
//! Still bounded, and never held open indefinitely the way an un-cancelled
//! stream would be, but worth stating accurately: an operator reading "30s"
//! and seeing a three-minute shutdown would reasonably conclude the bound had
//! failed.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::events::{Event as CoreEvent, EventKind as CoreEventKind};
use rmail_core::idempotency::IdempotencyStore;
use rmail_core::mail::{FullMessage, MailStore, MessageWithFlags, ThreadView};
use rmail_core::page::NEXT_PAGE_TOKEN_METADATA_KEY;
use rmail_core::repo::Attachment as CoreAttachment;
use rmail_core::Error;
use rmail_proto::v1::mail_service_server::MailService;
use rmail_proto::v1::{
    Attachment as ProtoAttachment, AttachmentChunk, CopyRequest, DeleteRequest,
    Event as ProtoEvent, EventKind as ProtoEventKind, FullMessage as ProtoFullMessage,
    GetAttachmentRequest, GetMessageRequest, GetThreadRequest, ListMessagesRequest,
    ListUnifiedRequest, Message as ProtoMessage, MoveRequest, SetFlagsRequest,
    Thread as ProtoThread, WatchEventsRequest,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Size of one `AttachmentChunk`'s `data`, well under the 16 MiB default
/// `grpc.limits.max_message_bytes` frame cap.
const ATTACHMENT_CHUNK_BYTES: usize = 256 * 1024;

/// How many chunks may sit between the producer and a client before
/// `GetAttachment` applies backpressure.
///
/// Small on purpose: the whole attachment is already resident in memory (see
/// [`rmail_core::mail::MailStore::attachment_bytes`]) by the time streaming
/// starts, so there is nothing to pipeline — this bound exists only to give a
/// slow client's backpressure somewhere to land instead of an unbounded queue
/// of pre-sliced chunks.
const ATTACHMENT_STREAM_BUFFER: usize = 4;

/// How many events may sit between the log and a client before `WatchEvents`
/// applies backpressure. See `rmaild::sync_service::STREAM_BUFFER` — the same
/// reasoning, duplicated because the two streams are independent.
const STREAM_BUFFER: usize = 256;

/// How many backlog events one durable read fetches while catching a client
/// up. See `rmaild::sync_service::REPLAY_PAGE`.
const REPLAY_PAGE: i64 = 500;

// The method paths the replay fence keys on. Written out rather than derived,
// because they are the same strings `auth::methods` matches and a mismatch
// here would silently give two RPCs one key namespace.
const MOVE_METHOD: &str = "/rmail.v1.MailService/Move";
const COPY_METHOD: &str = "/rmail.v1.MailService/Copy";
const SET_FLAGS_METHOD: &str = "/rmail.v1.MailService/SetFlags";
const DELETE_METHOD: &str = "/rmail.v1.MailService/Delete";

/// The `MailService` handler.
#[derive(Clone)]
pub struct MailApi {
    store: MailStore,
    /// The replay fence behind every mutation's `idempotency_key`.
    idempotency: IdempotencyStore,
    /// Cancelled when the daemon shuts down, so open streams stop with it
    /// rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl MailApi {
    /// Create a handler over a mail store.
    #[must_use]
    pub fn new(
        store: MailStore,
        idempotency: IdempotencyStore,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            store,
            idempotency,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl MailService for MailApi {
    type ListStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoMessage, Status>> + Send + 'static>>;

    async fn list(
        &self,
        request: Request<ListMessagesRequest>,
    ) -> Result<Response<Self::ListStream>, Status> {
        let req = request.into_inner();
        // A negative page size is nonsense rather than a request for the
        // default, and the other three list RPCs already say so — the same
        // input has to get the same answer on every one of them.
        if req.page_size < 0 {
            return Err(Status::from(Error::invalid_argument(
                "page_size must not be negative",
            )));
        }
        let page = self
            .store
            .list(req.mailbox_id, i64::from(req.page_size), &req.page_token)
            .await?;
        let items: Vec<Result<ProtoMessage, Status>> = page
            .messages
            .iter()
            .map(|m| Ok(message_to_proto(m)))
            .collect();
        let mut response: Response<Self::ListStream> =
            Response::new(Box::pin(tokio_stream::iter(items)));
        if let Some(token) = page.next_page_token {
            // Tokens are base64url by construction, so this parse cannot fail
            // — but a `Status` beats a panic if that ever stops being true,
            // and an unpaginated answer would be a silent truncation.
            let value = token.parse().map_err(|_| {
                Status::from(Error::internal("page token was not a valid header value"))
            })?;
            response
                .metadata_mut()
                .insert(NEXT_PAGE_TOKEN_METADATA_KEY, value);
        }
        Ok(response)
    }

    type ListUnifiedStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoMessage, Status>> + Send + 'static>>;

    /// The unified inbox, paginated exactly like [`MailService::list`] — same
    /// cap, same probe row, same `x-rmail-next-page-token` header, same
    /// "absence is final" contract.
    ///
    /// The one thing it deliberately does *not* do is filter or re-map the
    /// rows: each carries the `account_id`/`mailbox_id` it really has, which
    /// is what lets a client act on a unified row through the ordinary
    /// mutations with nothing unified-specific in the path.
    async fn list_unified(
        &self,
        request: Request<ListUnifiedRequest>,
    ) -> Result<Response<Self::ListUnifiedStream>, Status> {
        let req = request.into_inner();
        // Same answer `List` gives the same input: a negative page size is
        // nonsense, not a request for the default.
        if req.page_size < 0 {
            return Err(Status::from(Error::invalid_argument(
                "page_size must not be negative",
            )));
        }
        let page = self
            .store
            .list_unified(i64::from(req.page_size), &req.page_token)
            .await?;
        let items: Vec<Result<ProtoMessage, Status>> = page
            .messages
            .iter()
            .map(|m| Ok(message_to_proto(m)))
            .collect();
        let mut response: Response<Self::ListUnifiedStream> =
            Response::new(Box::pin(tokio_stream::iter(items)));
        if let Some(token) = page.next_page_token {
            let value = token.parse().map_err(|_| {
                Status::from(Error::internal("page token was not a valid header value"))
            })?;
            response
                .metadata_mut()
                .insert(NEXT_PAGE_TOKEN_METADATA_KEY, value);
        }
        Ok(response)
    }

    async fn get(
        &self,
        request: Request<GetMessageRequest>,
    ) -> Result<Response<ProtoFullMessage>, Status> {
        let id = request.into_inner().id;
        let full = self.store.get(id).await?;
        Ok(Response::new(full_message_to_proto(full)))
    }

    async fn get_thread(
        &self,
        request: Request<GetThreadRequest>,
    ) -> Result<Response<ProtoThread>, Status> {
        let id = request.into_inner().id;
        let view = self.store.get_thread(id).await?;
        Ok(Response::new(thread_to_proto(view)))
    }

    async fn r#move(&self, request: Request<MoveRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::idempotency::guard(
            &self.idempotency,
            MOVE_METHOD,
            &req.idempotency_key,
            &req,
            async {
                self.store
                    .move_message(req.message_id, req.dest_mailbox_id)
                    .await?;
                Ok(())
            },
        )
        .await
        .map(Response::new)
    }

    async fn copy(&self, request: Request<CopyRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::idempotency::guard(
            &self.idempotency,
            COPY_METHOD,
            &req.idempotency_key,
            &req,
            async {
                self.store
                    .copy_message(req.message_id, req.dest_mailbox_id)
                    .await?;
                Ok(())
            },
        )
        .await
        .map(Response::new)
    }

    async fn set_flags(&self, request: Request<SetFlagsRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::idempotency::guard(
            &self.idempotency,
            SET_FLAGS_METHOD,
            &req.idempotency_key,
            &req,
            async {
                self.store
                    .set_flags(req.message_id, req.flags.clone())
                    .await?;
                Ok(())
            },
        )
        .await
        .map(Response::new)
    }

    async fn delete(&self, request: Request<DeleteRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::idempotency::guard(
            &self.idempotency,
            DELETE_METHOD,
            &req.idempotency_key,
            &req,
            async {
                self.store.delete_message(req.message_id).await?;
                Ok(())
            },
        )
        .await
        .map(Response::new)
    }

    type GetAttachmentStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<AttachmentChunk, Status>> + Send + 'static>>;

    async fn get_attachment(
        &self,
        request: Request<GetAttachmentRequest>,
    ) -> Result<Response<Self::GetAttachmentStream>, Status> {
        let cancel = rpc_cancel(&self.shutdown);
        let req = request.into_inner();
        // Loaded once, up front — chunking below is pure slicing of an
        // already-resident buffer, not a reason to hold a database read open
        // for the life of the stream.
        let attachment = self
            .store
            .attachment_bytes(req.message_id, &req.part_id)
            .await?;

        let (tx, rx) = tokio::sync::mpsc::channel(ATTACHMENT_STREAM_BUFFER);
        tokio::spawn(
            async move {
                let total_size = i64::try_from(attachment.bytes.len()).unwrap_or(i64::MAX);
                let mut offset = 0usize;
                let mut first = true;
                loop {
                    let end = (offset + ATTACHMENT_CHUNK_BYTES).min(attachment.bytes.len());
                    let chunk = AttachmentChunk {
                        filename: first.then(|| attachment.filename.clone()).flatten(),
                        content_type: first.then(|| attachment.content_type.clone()).flatten(),
                        total_size: first.then_some(total_size),
                        data: attachment.bytes[offset..end].to_vec(),
                    };
                    first = false;
                    offset = end;
                    if send(&tx, &cancel, Ok(chunk)).await.is_break() {
                        return;
                    }
                    // A zero-byte attachment still gets exactly one chunk (the
                    // metadata one, with empty data) — this checks *after*
                    // sending so `offset == len == 0` still emits it.
                    if offset >= attachment.bytes.len() {
                        return;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type WatchEventsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoEvent, Status>> + Send + 'static>>;

    async fn watch_events(
        &self,
        request: Request<WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        let cancel = rpc_cancel(&self.shutdown);
        let req = request.into_inner();
        let filter = Filter::new(req.account_id, &req.kinds)?;

        if req.since_seq < 0 {
            return Err(Status::from(Error::invalid_argument(
                "since_seq must not be negative",
            )));
        }

        let log = self.store.events().clone();
        let mut catchup = log.catch_up(req.since_seq, REPLAY_PAGE).await?;

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                let mut cursor = req.since_seq;
                let mut page = std::mem::take(&mut catchup.backlog);
                let mut scanned_to = catchup.next_seq;

                'stream: loop {
                    loop {
                        let drained = page.is_empty();
                        for event in std::mem::take(&mut page) {
                            cursor = cursor.max(event.seq);
                            if !filter.admits(&event) {
                                continue;
                            }
                            if send(&tx, &cancel, Ok(to_proto(&event))).await.is_break() {
                                return;
                            }
                        }
                        cursor = cursor.max(scanned_to);
                        if drained {
                            break;
                        }
                        match log.since(cursor, REPLAY_PAGE).await {
                            Ok(next) => {
                                page = next.events;
                                scanned_to = next.next_seq;
                            }
                            Err(error) => {
                                let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                                return;
                            }
                        }
                    }

                    loop {
                        let received = tokio::select! {
                            () = cancel.cancelled() => {
                                crate::stream::terminate_cancelled(&tx).await;
                                return;
                            }
                            received = catchup.live.recv() => received,
                        };
                        match received {
                            Ok(event) => {
                                if event.seq <= cursor {
                                    continue;
                                }
                                cursor = event.seq;
                                if !filter.admits(&event) {
                                    continue;
                                }
                                if send(&tx, &cancel, Ok(to_proto(&event))).await.is_break() {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(missed)) => {
                                tracing::debug!(
                                    missed,
                                    cursor,
                                    "event stream lagged; re-reading from the log"
                                );
                                continue 'stream;
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// Which events a subscription wants. Identical to
/// `rmaild::sync_service::Filter` — see that module for the reasoning behind
/// each rule; duplicated rather than shared for the same reason the two
/// `watch_events` implementations are (see this module's docs).
struct Filter {
    account_id: Option<i64>,
    kinds: std::collections::HashSet<CoreEventKind>,
}

impl Filter {
    fn new(account_id: i64, kinds: &[i32]) -> Result<Self, Status> {
        let account_id = (account_id != 0).then_some(account_id);
        let resolved: std::collections::HashSet<CoreEventKind> = kinds
            .iter()
            .filter_map(|k| ProtoEventKind::try_from(*k).ok())
            .filter_map(from_proto_kind)
            .collect();
        if !kinds.is_empty() && resolved.is_empty() {
            return Err(Status::from(Error::invalid_argument(
                "no recognised event kinds in the filter",
            )));
        }
        Ok(Self {
            account_id,
            kinds: resolved,
        })
    }

    fn admits(&self, event: &CoreEvent) -> bool {
        if let Some(account_id) = self.account_id {
            if event.account_id != Some(account_id) {
                return false;
            }
        }
        self.kinds.is_empty() || self.kinds.contains(&event.kind)
    }
}

/// Send one stream item, giving up if the client went away or the daemon is
/// stopping. See `rmaild::sync_service::send` — identical reasoning.
async fn send<T>(
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    cancel: &CancellationToken,
    item: Result<T, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => {
            // Never end a cancelled stream silently — see `crate::stream`.
            crate::stream::terminate_cancelled(tx).await;
            std::ops::ControlFlow::Break(())
        }
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

/// The cancellation token an RPC's work runs under — a child of the daemon's
/// shutdown token. See `rmaild::sync_service::rpc_cancel`.
fn rpc_cancel(shutdown: &CancellationToken) -> CancellationToken {
    shutdown.child_token()
}

fn to_proto(event: &CoreEvent) -> ProtoEvent {
    ProtoEvent {
        seq: event.seq,
        kind: to_proto_kind(event.kind) as i32,
        at: event.at,
        account_id: event.account_id,
        mailbox_id: event.mailbox_id,
        message_id: event.message_id,
        payload: event.payload.to_string(),
    }
}

fn to_proto_kind(kind: CoreEventKind) -> ProtoEventKind {
    match kind {
        CoreEventKind::NewMail => ProtoEventKind::NewMail,
        CoreEventKind::FlagChanged => ProtoEventKind::FlagChanged,
        CoreEventKind::Moved => ProtoEventKind::Moved,
        CoreEventKind::Deleted => ProtoEventKind::Deleted,
        CoreEventKind::SyncState => ProtoEventKind::SyncState,
        CoreEventKind::SendResult => ProtoEventKind::SendResult,
        CoreEventKind::RuleFired => ProtoEventKind::RuleFired,
        CoreEventKind::AiSummary => ProtoEventKind::AiSummary,
    }
}

fn from_proto_kind(kind: ProtoEventKind) -> Option<CoreEventKind> {
    Some(match kind {
        ProtoEventKind::Unspecified => return None,
        ProtoEventKind::NewMail => CoreEventKind::NewMail,
        ProtoEventKind::FlagChanged => CoreEventKind::FlagChanged,
        ProtoEventKind::Moved => CoreEventKind::Moved,
        ProtoEventKind::Deleted => CoreEventKind::Deleted,
        ProtoEventKind::SyncState => CoreEventKind::SyncState,
        ProtoEventKind::SendResult => CoreEventKind::SendResult,
        ProtoEventKind::RuleFired => CoreEventKind::RuleFired,
        ProtoEventKind::AiSummary => CoreEventKind::AiSummary,
    })
}

fn message_to_proto(m: &MessageWithFlags) -> ProtoMessage {
    let msg = &m.message;
    ProtoMessage {
        id: msg.id,
        account_id: msg.account_id,
        mailbox_id: msg.mailbox_id,
        thread_id: msg.thread_id,
        message_id: msg.message_id.clone(),
        subject: msg.subject.clone(),
        from_addr: msg.from_addr.clone(),
        from_name: msg.from_name.clone(),
        to_addrs: msg.to_addrs.clone(),
        cc_addrs: msg.cc_addrs.clone(),
        date: msg.date,
        internaldate: msg.internaldate,
        size: msg.size,
        has_attachments: msg.has_attachments,
        flags: m.flags.clone(),
        created_at: msg.created_at,
        updated_at: msg.updated_at,
    }
}

fn attachment_to_proto(a: &CoreAttachment) -> ProtoAttachment {
    ProtoAttachment {
        id: a.id,
        part_id: a.part_id.clone().unwrap_or_default(),
        filename: a.filename.clone(),
        content_type: a.content_type.clone(),
        size: a.size,
        content_id: a.content_id.clone(),
        is_inline: a.is_inline,
    }
}

fn full_message_to_proto(full: FullMessage) -> ProtoFullMessage {
    let body_text = full.message.message.body_text.clone();
    let body_html = full.message.message.body_html.clone();
    let attachments = full.attachments.iter().map(attachment_to_proto).collect();
    ProtoFullMessage {
        message: Some(message_to_proto(&full.message)),
        body_text,
        body_html,
        attachments,
    }
}

fn thread_to_proto(view: ThreadView) -> ProtoThread {
    let t = &view.thread;
    ProtoThread {
        id: t.id,
        account_id: t.account_id,
        subject_norm: t.subject_norm.clone(),
        root_message_id: t.root_message_id,
        first_message_at: t.first_message_at,
        last_message_at: t.last_message_at,
        message_count: t.message_count,
        participants: t
            .participant_list()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        messages: view.messages.iter().map(message_to_proto).collect(),
        created_at: t.created_at,
        updated_at: t.updated_at,
    }
}
