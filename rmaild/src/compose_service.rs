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
#![allow(clippy::result_large_err)]

use rmail_core::compose::{
    Draft as CoreDraft, DraftAttachment as CoreAttachment, DraftPatch, DraftStore, Mailbox,
    NewAttachment, NewDraft,
};
use rmail_core::Error;
use rmail_proto::v1::compose_service_server::ComposeService;
use rmail_proto::v1::{
    CreateDraftRequest, DeleteDraftRequest, Draft as ProtoDraft, DraftAddress as ProtoAddress,
    DraftAttachment as ProtoAttachment, GetDraftRequest, ListDraftsRequest, ListDraftsResponse,
    NewDraftAttachment, RenderDraftRequest, RenderedDraft, UpdateDraftRequest,
};
use tonic::{Request, Response, Status};

/// The `ComposeService` handler.
#[derive(Clone)]
pub struct ComposeApi {
    store: DraftStore,
}

impl ComposeApi {
    /// Build a handler over a draft store.
    #[must_use]
    pub fn new(store: DraftStore) -> Self {
        Self { store }
    }
}

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
            .ok_or_else(|| Status::from(Error::invalid_argument("from is required")))?;
        let draft = self
            .store
            .create(NewDraft {
                account_id: req.account_id,
                from: mailbox_from_proto(&from)?,
                to: mailboxes_from_proto(&req.to)?,
                cc: mailboxes_from_proto(&req.cc)?,
                bcc: mailboxes_from_proto(&req.bcc)?,
                subject: req.subject,
                body_text: req.body_text,
                body_html: req.body_html,
                attachments: req
                    .attachments
                    .into_iter()
                    .map(attachment_from_proto)
                    .collect(),
                in_reply_to_message_id: req.in_reply_to_message_id,
            })
            .await?;
        Ok(Response::new(draft_to_proto(&draft)))
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
