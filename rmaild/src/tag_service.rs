//! The `TagService` gRPC implementation.
//!
//! Every RPC here is a thin translation over
//! [`rmail_core::tags::TagStore`] — see that module's own docs for the
//! domain contract (IMAP-first ordering, hierarchy/cycle rejection, the
//! `auto` downgrade, and the pending-suggestion state model
//! `SuggestTags`/`ResolveSuggestion` implement). This file's own job is
//! proto <-> domain conversion and mapping [`rmail_core::Error`] to
//! [`tonic::Status`] (via `?`, using the crate's existing `From<Error> for
//! Status` — see `mail_service`/`account_service` for the identical
//! pattern).
//!
//! `AddTag` always applies with [`TagSource::User`] — an RPC call is, by
//! definition, an external caller acting on the mailbox, the same class of
//! actor `source = 'user'` names for a direct `mail tag` invocation. A
//! `source = 'ai'`/`'rule'`/`'imap'` row is never created through this RPC:
//! AI suggestions land `pending` via task 57's own job (calling
//! [`rmail_core::tags::TagStore`] directly, in-process), and imported IMAP
//! keywords are meant to land via
//! [`rmail_core::tags::TagStore::import_imap_keywords`] — a real, tested
//! primitive (task 55's acceptance: "inbound server keywords import as
//! `source='imap'` tags") that this service deliberately does *not* expose
//! as its own RPC (task 55's acceptance list names `Add/Remove/List/Create/
//! BulkTag/SuggestTags/ResolveSuggestion` — no `ImportKeywords`). What task
//! 55 does *not* do is call it from anywhere: wiring it to run automatically
//! — after a sync persists new flags, or on a periodic reconciliation pass
//! — means touching `rmail-core::sync`'s delta/full-sync internals, which
//! this task's scope deliberately stays out of. Until something calls it,
//! `import_imap_keywords` is a correct, callable seam, not yet a running
//! pipeline stage.
//!
//! `SuggestTags` is bounded, not truly streamed, the same way
//! `MailService::list` is (see that module's own docs): a message has at
//! most a handful of pending suggestions, so collecting them into a `Vec`
//! before wrapping in [`tokio_stream::iter`] costs nothing in practice,
//! while keeping the RPC's wire shape (`stream TagSuggestion`) ready for a
//! future truly-incremental producer without a breaking change.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::tags::{
    self, BulkSelector, PendingSuggestion, Tag, TagApplication, TagStore, TagWithCount,
};
use rmail_core::Error;
use rmail_proto::v1::tag_service_server::TagService;
use rmail_proto::v1::{
    bulk_tag_request, target, AddTagRequest, AddTagResponse, BulkTagRequest, BulkTagResponse,
    CreateTagRequest, ListTagsRequest, ListTagsResponse, RemoveTagRequest,
    ResolveSuggestionRequest, SuggestTagsRequest, Tag as ProtoTag,
    TagApplication as ProtoTagApplication, TagSource as ProtoTagSource, TagSuggestion,
    TagSyncMode as ProtoTagSyncMode, TagWithCount as ProtoTagWithCount, Target as ProtoTarget,
};
use tonic::{Request, Response, Status};

/// The `TagService` handler, backed by a [`TagStore`].
#[derive(Clone)]
pub struct TagApi {
    store: TagStore,
}

impl TagApi {
    /// Create a handler over a tag store.
    #[must_use]
    pub fn new(store: TagStore) -> Self {
        Self { store }
    }
}

#[tonic::async_trait]
impl TagService for TagApi {
    async fn add_tag(
        &self,
        request: Request<AddTagRequest>,
    ) -> Result<Response<AddTagResponse>, Status> {
        let req = request.into_inner();
        let target = target_from_proto(req.target)?;
        let applications = self
            .store
            .add_tag(target, &req.names, tags::TagSource::User)
            .await?;
        Ok(Response::new(AddTagResponse {
            applications: applications.iter().map(application_to_proto).collect(),
        }))
    }

    async fn remove_tag(&self, request: Request<RemoveTagRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let target = target_from_proto(req.target)?;
        self.store.remove_tag(target, &req.names).await?;
        Ok(Response::new(()))
    }

    async fn list_tags(
        &self,
        request: Request<ListTagsRequest>,
    ) -> Result<Response<ListTagsResponse>, Status> {
        let account_id = request.into_inner().account_id;
        let tags = self.store.list_tags(account_id).await?;
        Ok(Response::new(ListTagsResponse {
            tags: tags.iter().map(tag_with_count_to_proto).collect(),
        }))
    }

    async fn create_tag(
        &self,
        request: Request<CreateTagRequest>,
    ) -> Result<Response<ProtoTag>, Status> {
        let req = request.into_inner();
        let sync_mode = req.sync_mode.and_then(sync_mode_from_proto);
        let tag = self
            .store
            .create_tag(
                req.account_id,
                &req.name,
                req.color,
                sync_mode,
                req.parent_id,
            )
            .await?;
        Ok(Response::new(tag_to_proto(&tag)))
    }

    async fn bulk_tag(
        &self,
        request: Request<BulkTagRequest>,
    ) -> Result<Response<BulkTagResponse>, Status> {
        let req = request.into_inner();
        let selector = selector_from_proto(req.selector)?;
        let outcome = self
            .store
            .bulk_tag(req.account_id, selector, &req.names)
            .await?;
        Ok(Response::new(BulkTagResponse {
            message_count: usize_to_i64(outcome.message_count),
            applied: usize_to_i64(outcome.applied),
        }))
    }

    type SuggestTagsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<TagSuggestion, Status>> + Send + 'static>>;

    async fn suggest_tags(
        &self,
        request: Request<SuggestTagsRequest>,
    ) -> Result<Response<Self::SuggestTagsStream>, Status> {
        let message_id = request.into_inner().message_id;
        let pending = self.store.list_pending_suggestions(message_id).await?;
        let items: Vec<Result<TagSuggestion, Status>> =
            pending.iter().map(|p| Ok(suggestion_to_proto(p))).collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(items))))
    }

    async fn resolve_suggestion(
        &self,
        request: Request<ResolveSuggestionRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.store
            .resolve_suggestion(req.message_tag_id, req.accept)
            .await?;
        Ok(Response::new(()))
    }
}

/// `usize -> i64`, saturating rather than panicking on a value past `i64::MAX`
/// — unreachable in practice (`usize` message/apply counts this small), but
/// saturating keeps this boundary panic-free regardless.
fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn target_from_proto(target: Option<ProtoTarget>) -> Result<tags::Target, Status> {
    match target.and_then(|t| t.of) {
        Some(target::Of::MessageId(id)) => Ok(tags::Target::Message(id)),
        Some(target::Of::ThreadId(id)) => Ok(tags::Target::Thread(id)),
        None => Err(Status::from(Error::invalid_argument(
            "target is required (message_id or thread_id)",
        ))),
    }
}

fn target_to_proto(target: tags::Target) -> ProtoTarget {
    ProtoTarget {
        of: Some(match target {
            tags::Target::Message(id) => target::Of::MessageId(id),
            tags::Target::Thread(id) => target::Of::ThreadId(id),
        }),
    }
}

fn selector_from_proto(
    selector: Option<bulk_tag_request::Selector>,
) -> Result<BulkSelector, Status> {
    match selector {
        Some(bulk_tag_request::Selector::Query(query)) => Ok(BulkSelector::Query(query)),
        Some(bulk_tag_request::Selector::MessageIds(ids)) => Ok(BulkSelector::MessageIds(ids.ids)),
        None => Err(Status::from(Error::invalid_argument(
            "bulk_tag requires a query or message_ids selector",
        ))),
    }
}

fn sync_mode_to_proto(mode: rmail_core::config::TagSyncMode) -> ProtoTagSyncMode {
    use rmail_core::config::TagSyncMode;
    match mode {
        TagSyncMode::Local => ProtoTagSyncMode::Local,
        TagSyncMode::Imap => ProtoTagSyncMode::Imap,
        TagSyncMode::Auto => ProtoTagSyncMode::Auto,
    }
}

/// An out-of-range or `UNSPECIFIED` wire value degrades to `None` ("leave
/// unset" / "use the default"), the same fallback [`CreateTag`](TagService::
/// create_tag)'s domain layer already applies to an absent `sync_mode` —
/// never a hard error over a caller that simply didn't set the field.
fn sync_mode_from_proto(mode: i32) -> Option<rmail_core::config::TagSyncMode> {
    use rmail_core::config::TagSyncMode;
    match ProtoTagSyncMode::try_from(mode).ok()? {
        ProtoTagSyncMode::Unspecified => None,
        ProtoTagSyncMode::Local => Some(TagSyncMode::Local),
        ProtoTagSyncMode::Imap => Some(TagSyncMode::Imap),
        ProtoTagSyncMode::Auto => Some(TagSyncMode::Auto),
    }
}

fn source_to_proto(source: tags::TagSource) -> ProtoTagSource {
    match source {
        tags::TagSource::User => ProtoTagSource::User,
        tags::TagSource::Ai => ProtoTagSource::Ai,
        tags::TagSource::Rule => ProtoTagSource::Rule,
        tags::TagSource::Imap => ProtoTagSource::Imap,
    }
}

fn tag_to_proto(tag: &Tag) -> ProtoTag {
    ProtoTag {
        id: tag.id,
        account_id: tag.account_id,
        name: tag.name.clone(),
        parent_id: tag.parent_id,
        color: tag.color.clone(),
        sync_mode: sync_mode_to_proto(tag.sync_mode) as i32,
        created_at: tag.created_at,
    }
}

fn tag_with_count_to_proto(with_count: &TagWithCount) -> ProtoTagWithCount {
    ProtoTagWithCount {
        tag: Some(tag_to_proto(&with_count.tag)),
        message_count: with_count.message_count,
    }
}

fn application_to_proto(application: &TagApplication) -> ProtoTagApplication {
    ProtoTagApplication {
        id: application.id,
        tag: Some(tag_to_proto(&application.tag)),
        target: Some(target_to_proto(application.target)),
        source: source_to_proto(application.source) as i32,
    }
}

fn suggestion_to_proto(pending: &PendingSuggestion) -> TagSuggestion {
    TagSuggestion {
        message_tag_id: pending.message_tag.id,
        tag: Some(tag_to_proto(&pending.tag)),
        confidence: pending.message_tag.confidence.unwrap_or(0.0),
        rationale: pending.message_tag.rationale.clone().unwrap_or_default(),
    }
}
