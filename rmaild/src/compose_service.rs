//! The `ComposeService` gRPC implementation.
//!
//! A thin translation over [`rmail_core::compose::DraftStore`] — the store
//! owns draft CRUD, the frozen reply-threading headers, and the bounds on
//! subject/filename/attachment size; [`rmail_core::compose::mime`] owns
//! rendering. This file's only real work is the proto⇄domain mapping, and the
//! one place that is not mechanical is the patch semantics:
//!
//! # Why the update request wraps its lists
//!
//! proto3 cannot tell an absent `repeated` field from an empty one, and for
//! `UpdateDraft` that distinction is the whole contract: an absent `to` means
//! "leave the To header alone", an empty one means "clear it". Wrapping each
//! list in a message ([`DraftAddressList`], [`DraftAttachmentList`]) restores
//! the presence bit — the same trick `google.protobuf.StringValue` exists for
//! — so the two intents stay distinguishable on the wire instead of being
//! collapsed into one by the encoding.
//!
//! # RenderDraft sends nothing
//!
//! It serializes and returns; SMTP submission is task 61. It nonetheless
//! sits behind `mail.send` in `auth::methods` — see that table's row for the
//! reasoning.
//!
//! # The AI half (task 62) is an adapter and nothing else
//!
//! `DraftReply`/`RewriteDraft`/`ListDraftRevisions`/`SelectDraftRevision` have
//! none of their logic here. [`rmail_core::compose::reply`] owns context
//! gathering, the AI policy/budget gate, the fences, the redaction firewall,
//! the model call, header derivation and the revision history; this file
//! converts its [`ReplyEvent`](rmail_core::compose::reply::ReplyEvent)s to
//! wire frames and its enums to proto enums. That split is deliberate for the
//! same reason `ai_service`'s `AskMailbox` adapter documents: every property
//! task 62 has to guarantee — above all that a drafted reply can never send
//! itself — is provable without a gRPC server, and a transport layer that
//! could weaken one by accident is one that would need re-auditing on every
//! change.
//!
//! [`ComposeApi::drafter`] is `Option`, and absent on a daemon whose AI
//! subsystem never came up. The four RPCs then answer `FAILED_PRECONDITION`
//! rather than reaching a `NullProvider` that would refuse anyway after
//! spending a policy resolution and a redaction pass — the same shape
//! `SendSchedulerApi::guardian` uses.
#![allow(clippy::result_large_err)]

use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use rmail_core::compose::reply::{
    self, Length, ReplyDrafter, ReplyEvent, ReplyRequest, Revision, RewriteRequest, Tone,
};
use rmail_core::compose::{
    Draft as CoreDraft, DraftAttachment as CoreAttachment, DraftPatch, DraftStore, Mailbox,
    NewAttachment, NewDraft,
};
use rmail_core::idempotency::IdempotencyStore;
use rmail_core::{Database, Error};
use rmail_proto::v1::compose_service_server::ComposeService;
use rmail_proto::v1::{
    draft_reply_event, AiUsage, CreateDraftRequest, DeleteDraftRequest, Draft as ProtoDraft,
    DraftAddress as ProtoAddress, DraftAttachment as ProtoAttachment, DraftReplyContext,
    DraftReplyDone, DraftReplyEvent, DraftReplyRequest, DraftRevision as ProtoRevision,
    GetDraftRequest, ListDraftRevisionsRequest, ListDraftRevisionsResponse, ListDraftsRequest,
    ListDraftsResponse, NewDraftAttachment, RenderDraftRequest, RenderedDraft, RewriteDraftRequest,
    RewriteLength, RewriteTone, SelectDraftRevisionRequest, UpdateDraftRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Backpressure between a stream's producer task and its consumer — see
/// `sync_service::STREAM_BUFFER`'s identical reasoning.
const STREAM_BUFFER: usize = 64;

/// The `ComposeService` handler.
#[derive(Clone)]
pub struct ComposeApi {
    store: DraftStore,
    /// The replay fence behind `CreateDraft`'s `idempotency_key`. `drafts`
    /// carries no uniqueness of its own, so without this a retried create
    /// leaves two identical drafts and no way to tell which id is live.
    idempotency: IdempotencyStore,
    /// Read directly by the revision RPCs, which call no model and must keep
    /// working on a daemon whose AI subsystem is off: a user who turned AI off
    /// after a rewrite must still be able to revert it.
    db: Database,
    /// `None` on a daemon whose AI subsystem never came up — see the module
    /// docs.
    drafter: Option<Arc<ReplyDrafter>>,
    /// Cancelled when the daemon starts shutting down, so a `DraftReply`
    /// stream ends (and aborts its upstream request) instead of holding the
    /// connection open through graceful shutdown.
    shutdown: CancellationToken,
}

impl ComposeApi {
    /// Build a handler over a draft store.
    ///
    /// The AI half is attached separately with [`Self::with_drafter`]: a
    /// daemon can serve drafts long before (or entirely without) a provider.
    #[must_use]
    pub fn new(
        store: DraftStore,
        idempotency: IdempotencyStore,
        db: Database,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            store,
            idempotency,
            db,
            drafter: None,
            shutdown,
        }
    }

    /// Attach the reply drafter, enabling `DraftReply`/`RewriteDraft`.
    #[must_use]
    pub fn with_drafter(mut self, drafter: ReplyDrafter) -> Self {
        self.drafter = Some(Arc::new(drafter));
        self
    }

    /// The drafter, or the refusal a daemon with no AI subsystem owes.
    fn drafter(&self) -> Result<&Arc<ReplyDrafter>, Status> {
        self.drafter.as_ref().ok_or_else(|| {
            Status::from(Error::failed_precondition(
                "AI reply drafting is not available on this daemon: the AI subsystem is \
                 disabled or its provider could not be built"
                    .to_owned(),
            ))
        })
    }
}

const CREATE_DRAFT_METHOD: &str = "/rmail.v1.ComposeService/CreateDraft";

#[tonic::async_trait]
impl ComposeService for ComposeApi {
    async fn create_draft(
        &self,
        request: Request<CreateDraftRequest>,
    ) -> Result<Response<ProtoDraft>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        let from = req
            .from
            .clone()
            .ok_or_else(|| Status::from(Error::invalid_argument("from is required")))?;
        let new = NewDraft {
            account_id: req.account_id,
            from: mailbox_from_proto(&from)?,
            to: mailboxes_from_proto(&req.to)?,
            cc: mailboxes_from_proto(&req.cc)?,
            bcc: mailboxes_from_proto(&req.bcc)?,
            subject: req.subject.clone(),
            body_text: req.body_text.clone(),
            body_html: req.body_html.clone(),
            attachments: req
                .attachments
                .iter()
                .cloned()
                .map(attachment_from_proto)
                .collect(),
            in_reply_to_message_id: req.in_reply_to_message_id,
        };
        crate::idempotency::guard(
            &self.idempotency,
            CREATE_DRAFT_METHOD,
            &req.idempotency_key,
            &req,
            async {
                let draft = self.store.create(new).await?;
                Ok(draft_to_proto(&draft))
            },
        )
        .await
        .map(Response::new)
    }

    async fn get_draft(
        &self,
        request: Request<GetDraftRequest>,
    ) -> Result<Response<ProtoDraft>, Status> {
        let draft = self.store.get(request.into_inner().draft_id).await?;
        Ok(Response::new(draft_to_proto(&draft)))
    }

    async fn list_drafts(
        &self,
        request: Request<ListDraftsRequest>,
    ) -> Result<Response<ListDraftsResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        // A negative page size is nonsense rather than a request for zero, so
        // it is rejected instead of silently becoming the default.
        let page_size = usize::try_from(req.page_size)
            .map_err(|_| Status::from(Error::invalid_argument("page_size must not be negative")))?;
        let page = self
            .store
            .list(req.account_id, page_size, &req.page_token)
            .await?;
        Ok(Response::new(ListDraftsResponse {
            drafts: page.drafts.iter().map(draft_to_proto).collect(),
            next_page_token: page.next_page_token.unwrap_or_default(),
        }))
    }

    async fn update_draft(
        &self,
        request: Request<UpdateDraftRequest>,
    ) -> Result<Response<ProtoDraft>, Status> {
        let req = request.into_inner();
        let patch = DraftPatch {
            from: req.from.as_ref().map(mailbox_from_proto).transpose()?,
            to: req
                .to
                .map(|list| mailboxes_from_proto(&list.addresses))
                .transpose()?,
            cc: req
                .cc
                .map(|list| mailboxes_from_proto(&list.addresses))
                .transpose()?,
            bcc: req
                .bcc
                .map(|list| mailboxes_from_proto(&list.addresses))
                .transpose()?,
            subject: req.subject,
            body_text: req.body_text,
            body_html: req.body_html,
            attachments: req.attachments.map(|list| {
                list.attachments
                    .into_iter()
                    .map(attachment_from_proto)
                    .collect()
            }),
        };
        let draft = self.store.update(req.draft_id, patch).await?;
        Ok(Response::new(draft_to_proto(&draft)))
    }

    async fn delete_draft(
        &self,
        request: Request<DeleteDraftRequest>,
    ) -> Result<Response<()>, Status> {
        self.store.delete(request.into_inner().draft_id).await?;
        Ok(Response::new(()))
    }

    async fn render_draft(
        &self,
        request: Request<RenderDraftRequest>,
    ) -> Result<Response<RenderedDraft>, Status> {
        let rendered = self.store.render(request.into_inner().draft_id).await?;
        Ok(Response::new(RenderedDraft {
            mime: rendered.mime,
            message_id: rendered.message_id,
            envelope_recipients: rendered.envelope_recipients,
        }))
    }

    type DraftReplyStream = Pin<Box<dyn Stream<Item = Result<DraftReplyEvent, Status>> + Send>>;

    #[tracing::instrument(skip(self, request), fields(message_id, reply_all))]
    async fn draft_reply(
        &self,
        request: Request<DraftReplyRequest>,
    ) -> Result<Response<Self::DraftReplyStream>, Status> {
        let drafter = Arc::clone(self.drafter()?);
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("message_id", req.message_id);
        span.record("reply_all", req.reply_all);

        // A child of the daemon's shutdown token: when the client disconnects
        // this is cancelled by the send loop below, which drops the core
        // stream, which aborts the upstream HTTP request rather than merely
        // abandoning the local relay.
        let cancel = self.shutdown.child_token();
        // Awaited here, not in the producer task, so everything decidable
        // without the network — a missing message, an over-long intent, a
        // policy refusal, a spent budget — reaches the client as the RPC's own
        // status instead of as an error frame it has to unwrap.
        let mut events = drafter
            .draft_reply(
                &ReplyRequest {
                    message_id: req.message_id,
                    intent: req.intent,
                    reply_all: req.reply_all,
                },
                &cancel,
            )
            .await
            .map_err(Status::from)?;

        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                // Cancelled when this task ends for any reason. Dropping
                // `events` alone would eventually stop the core drafter, but
                // only once it tried to send again; the token is what aborts
                // the upstream HTTP request immediately.
                let _guard = cancel.clone().drop_guard();
                loop {
                    let event = tokio::select! {
                        // Watched alongside the stream, not only discovered by
                        // a failing `send`: `DraftReply` has no server-side
                        // deadline by design, and `ClaudeProvider::stream`
                        // applies no per-request timeout, so a stalled upstream
                        // plus a client that has gone away would otherwise hold
                        // a shared `ai.limits` permit until daemon shutdown.
                        () = tx.closed() => return,
                        next = events.next() => next,
                    };
                    let Some(event) = event else { return };
                    let frame = match event {
                        Ok(event) => Ok(event_to_proto(event)),
                        Err(error) => Err(Status::from(error)),
                    };
                    let terminal = frame.is_err();
                    if tx.send(frame).await.is_err() || terminal {
                        return;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    #[tracing::instrument(skip(self, request), fields(draft_id))]
    async fn rewrite_draft(
        &self,
        request: Request<RewriteDraftRequest>,
    ) -> Result<Response<ProtoRevision>, Status> {
        let drafter = self.drafter()?;
        let req = request.into_inner();
        tracing::Span::current().record("draft_id", req.draft_id);
        let revision = drafter
            .rewrite(
                &RewriteRequest {
                    draft_id: req.draft_id,
                    tone: tone_from_proto(req.tone)?,
                    length: length_from_proto(req.length)?,
                    instruction: req.instruction,
                },
                &self.shutdown.child_token(),
            )
            .await?;
        Ok(Response::new(revision_to_proto(&revision)))
    }

    async fn list_draft_revisions(
        &self,
        request: Request<ListDraftRevisionsRequest>,
    ) -> Result<Response<ListDraftRevisionsResponse>, Status> {
        let revisions = reply::list_revisions(&self.db, request.into_inner().draft_id).await?;
        Ok(Response::new(ListDraftRevisionsResponse {
            revisions: revisions.iter().map(revision_to_proto).collect(),
        }))
    }

    async fn select_draft_revision(
        &self,
        request: Request<SelectDraftRevisionRequest>,
    ) -> Result<Response<ProtoDraft>, Status> {
        let req = request.into_inner();
        if req.seq < 0 {
            return Err(Status::from(Error::invalid_argument(
                "a revision sequence must not be negative",
            )));
        }
        let draft = reply::select_revision(&self.db, req.draft_id, req.seq).await?;
        Ok(Response::new(draft_to_proto(&draft)))
    }
}

/// One core event as its wire frame.
fn event_to_proto(event: ReplyEvent) -> DraftReplyEvent {
    let event = match event {
        ReplyEvent::Context(context) => draft_reply_event::Event::Context(DraftReplyContext {
            thread_messages: i32::try_from(context.thread_messages).unwrap_or(i32::MAX),
            withheld_by_policy: i32::try_from(context.withheld_by_policy).unwrap_or(i32::MAX),
            voice_samples: i32::try_from(context.voice_samples).unwrap_or(i32::MAX),
            model: context.model,
        }),
        ReplyEvent::Token(token) => draft_reply_event::Event::Token(token),
        ReplyEvent::Drafted(draft) => draft_reply_event::Event::Draft(draft_to_proto(&draft)),
        ReplyEvent::Usage(usage) => draft_reply_event::Event::Usage(AiUsage {
            input_tokens: i64::from(usage.input_tokens),
            output_tokens: i64::from(usage.output_tokens),
            cache_creation_input_tokens: i64::from(usage.cache_creation_input_tokens),
            cache_read_input_tokens: i64::from(usage.cache_read_input_tokens),
        }),
        ReplyEvent::Done(stop_reason) => draft_reply_event::Event::Done(DraftReplyDone {
            stop_reason: stop_reason.as_str().to_owned(),
        }),
    };
    DraftReplyEvent { event: Some(event) }
}

/// The wire tone as a domain tone.
///
/// `UNSPECIFIED` maps to [`Tone::AsIs`] rather than being rejected: proto3
/// cannot tell "unset" from "zero", so a client that only wants a length
/// change sends no tone at all, and refusing that would make the two knobs
/// mandatory together. Asking for *nothing* is still refused — by
/// `RewriteRequest`'s own check, which is where it belongs, since a CLI can
/// reach the same state without going through this function.
fn tone_from_proto(value: i32) -> Result<Tone, Status> {
    match RewriteTone::try_from(value) {
        Ok(RewriteTone::Unspecified | RewriteTone::AsIs) => Ok(Tone::AsIs),
        Ok(RewriteTone::Formal) => Ok(Tone::Formal),
        Ok(RewriteTone::Casual) => Ok(Tone::Casual),
        Ok(RewriteTone::Warmer) => Ok(Tone::Warmer),
        Ok(RewriteTone::Firmer) => Ok(Tone::Firmer),
        Ok(RewriteTone::Mirror) => Ok(Tone::MirrorRecipient),
        Err(_) => Err(Status::from(Error::invalid_argument(format!(
            "unknown rewrite tone {value}"
        )))),
    }
}

fn length_from_proto(value: i32) -> Result<Length, Status> {
    match RewriteLength::try_from(value) {
        Ok(RewriteLength::Unspecified | RewriteLength::AsIs) => Ok(Length::AsIs),
        Ok(RewriteLength::Shorter) => Ok(Length::Shorter),
        Ok(RewriteLength::Longer) => Ok(Length::Longer),
        Err(_) => Err(Status::from(Error::invalid_argument(format!(
            "unknown rewrite length {value}"
        )))),
    }
}

fn revision_to_proto(revision: &Revision) -> ProtoRevision {
    ProtoRevision {
        id: revision.id,
        draft_id: revision.draft_id,
        seq: revision.seq,
        label: revision.label.clone(),
        subject: revision.subject.clone(),
        body_text: revision.body_text.clone(),
        model: revision.model.clone(),
        active: revision.active,
        created_at: revision.created_at,
    }
}

/// A proto address as a validated [`Mailbox`].
///
/// The address is taken as an addr-spec, not parsed for angle brackets: the
/// wire shape already separates the display name, so accepting
/// `"Alice <a@x>"` in the `address` field would mean two ways to say the same
/// thing and one of them silently winning.
fn mailbox_from_proto(address: &ProtoAddress) -> Result<Mailbox, Status> {
    let display_name = Some(address.display_name.as_str()).filter(|n| !n.is_empty());
    Ok(Mailbox::new(&address.address, display_name)?)
}

fn mailboxes_from_proto(addresses: &[ProtoAddress]) -> Result<Vec<Mailbox>, Status> {
    addresses.iter().map(mailbox_from_proto).collect()
}

fn attachment_from_proto(attachment: NewDraftAttachment) -> NewAttachment {
    NewAttachment {
        filename: attachment.filename,
        content_type: attachment.content_type,
        content: attachment.content,
    }
}

fn address_to_proto(mailbox: &Mailbox) -> ProtoAddress {
    ProtoAddress {
        address: mailbox.address().to_owned(),
        display_name: mailbox.display_name().unwrap_or_default().to_owned(),
    }
}

fn attachment_to_proto(attachment: &CoreAttachment) -> ProtoAttachment {
    ProtoAttachment {
        id: attachment.id,
        filename: attachment.filename.clone(),
        content_type: attachment.content_type.clone(),
        size: attachment.size,
        content: attachment.content.clone(),
    }
}

fn draft_to_proto(draft: &CoreDraft) -> ProtoDraft {
    ProtoDraft {
        id: draft.id,
        account_id: draft.account_id,
        from: Some(address_to_proto(&draft.from)),
        to: draft.to.iter().map(address_to_proto).collect(),
        cc: draft.cc.iter().map(address_to_proto).collect(),
        bcc: draft.bcc.iter().map(address_to_proto).collect(),
        subject: draft.subject.clone(),
        body_text: draft.body_text.clone(),
        body_html: draft.body_html.clone(),
        attachments: draft.attachments.iter().map(attachment_to_proto).collect(),
        in_reply_to_message_id: draft.in_reply_to_message_id,
        in_reply_to: draft.in_reply_to.clone(),
        references: draft.references.clone(),
        created_at: draft.created_at,
        updated_at: draft.updated_at,
    }
}
