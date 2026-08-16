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
//! `SuggestTags` grew a real producer in task 57. With a
//! [`SuggestionEngine`] wired in (see [`TagApi::with_suggestions`]) it sends
//! whatever is already `pending` first, then classifies the message and sends
//! each new suggestion as it is decided and written — so the wire shape
//! (`stream TagSuggestion`) is now load-bearing rather than reserved. Without
//! one — a daemon whose AI subsystem is off — it stays the bounded replay task
//! 55 shipped: the background pass's work is still readable, nothing new is
//! generated. See [`rmail_core::tags::ai::SuggestionEngine`] for precisely
//! what "streams as Claude responds" does and does not mean here.
//!
//! That change moved the RPC out of the read half of the scope table
//! (`rmaild::auth::methods`) and out of `effect: Read` in
//! `rmail_core::parity`: it now spends a model call and writes
//! `message_tags`. Both are asserted against each other by the agreement test
//! in `auth::methods`' own suite.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::tags::ai::{self, SuggestionEngine};
use rmail_core::tags::{
    self, BulkSelector, PendingSuggestion, Tag, TagApplication, TagStore, TagWithCount,
};
use rmail_core::Error;
use rmail_proto::v1::tag_service_server::TagService;
use rmail_proto::v1::{
    bulk_tag_request, target, AddTagRequest, AddTagResponse, BulkTagRequest, BulkTagResponse,
    CreateTagRequest, ListTagRulesRequest, ListTagRulesResponse, ListTagsRequest, ListTagsResponse,
    RemoveTagRequest, ResolveSuggestionRequest, SetTagRuleRequest, SuggestTagsRequest,
    Tag as ProtoTag, TagApplication as ProtoTagApplication, TagRule as ProtoTagRule,
    TagRuleMode as ProtoTagRuleMode, TagSource as ProtoTagSource, TagSuggestion,
    TagSyncMode as ProtoTagSyncMode, TagWithCount as ProtoTagWithCount, Target as ProtoTarget,
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `TagService` handler, backed by a [`TagStore`].
#[derive(Clone)]
pub struct TagApi {
    store: TagStore,
    /// The live classifier behind `SuggestTags`, when the daemon's AI
    /// subsystem is usable — see [`TagApi::with_suggestions`]. `None` leaves
    /// `SuggestTags` the pending-only replay task 55 shipped, which is the
    /// right behaviour for a daemon with AI switched off: the background
    /// pass's work is still readable, nothing new is generated.
    suggestions: Option<SuggestionEngine>,
    /// Parent of the per-request token each `SuggestTags` classification runs
    /// under, so daemon shutdown cancels every one of them. A client that
    /// hangs up is handled a level down, in
    /// [`rmail_core::tags::ai::SuggestionEngine::suggest`], which races the
    /// whole call against the response channel closing.
    stopping: CancellationToken,
}

impl TagApi {
    /// Create a handler over a tag store, with no live classifier.
    #[must_use]
    pub fn new(store: TagStore) -> Self {
        Self {
            store,
            suggestions: None,
            stopping: CancellationToken::new(),
        }
    }

    /// Let `SuggestTags` classify a message it has no pending suggestions for,
    /// streaming each new one as it lands (see
    /// [`rmail_core::tags::ai::SuggestionEngine`] for exactly what "streams as
    /// Claude responds" means, and for the gate ordering the call goes
    /// through).
    #[must_use]
    pub fn with_suggestions(
        mut self,
        engine: SuggestionEngine,
        stopping: CancellationToken,
    ) -> Self {
        self.suggestions = Some(engine);
        self.stopping = stopping;
        self
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
        let Some(engine) = self.suggestions.as_ref() else {
            // No AI subsystem: replay what the background pass already wrote.
            let pending = self.store.list_pending_suggestions(message_id).await?;
            let items: Vec<Result<TagSuggestion, Status>> =
                pending.iter().map(|p| Ok(suggestion_to_proto(p))).collect();
            return Ok(Response::new(Box::pin(tokio_stream::iter(items))));
        };
        // A child token, the same shape every other streaming/AI RPC in this
        // daemon uses (`ai_service`, `notification_service`, `search_service`,
        // `finder_service`): daemon shutdown still cancels it, and the
        // classification is scoped to this one request rather than sharing a
        // token with every other in flight.
        let stream = engine
            .suggest(message_id, &self.stopping.child_token())
            .await?;
        Ok(Response::new(Box::pin(stream.map(|item| {
            item.map(|p| suggestion_to_proto(&p)).map_err(Status::from)
        }))))
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

    #[tracing::instrument(skip(self, request), fields(account_id, mode), err)]
    async fn set_tag_rule(
        &self,
        request: Request<SetTagRuleRequest>,
    ) -> Result<Response<ProtoTagRule>, Status> {
        let req = request.into_inner();
        // An unspecified mode is the caller not saying, which must not be
        // read as "auto" — the whole point of the rule is that applying a
        // tag without asking is the privileged half.
        let mode = match ProtoTagRuleMode::try_from(req.mode).unwrap_or_default() {
            ProtoTagRuleMode::Auto => ai::TagRuleMode::Auto,
            ProtoTagRuleMode::Suggest | ProtoTagRuleMode::Unspecified => ai::TagRuleMode::Suggest,
        };
        tracing::Span::current()
            .record("account_id", req.account_id)
            .record("mode", mode.as_str());
        let rule = self
            .store
            .set_tag_rule(
                req.account_id,
                &req.name,
                &req.tag_name,
                mode,
                req.min_conf,
                req.enabled,
            )
            .await?;
        Ok(Response::new(tag_rule_to_proto(&rule)))
    }

    #[tracing::instrument(skip(self, request), fields(account_id), err)]
    async fn list_tag_rules(
        &self,
        request: Request<ListTagRulesRequest>,
    ) -> Result<Response<ListTagRulesResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("account_id", req.account_id);
        let rules = self.store.list_tag_rules(req.account_id).await?;
        Ok(Response::new(ListTagRulesResponse {
            rules: rules.iter().map(tag_rule_to_proto).collect(),
        }))
    }
}

fn tag_rule_to_proto(rule: &ai::TagRule) -> ProtoTagRule {
    ProtoTagRule {
        id: rule.id,
        account_id: rule.account_id,
        name: rule.name.clone(),
        tag_id: rule.tag_id,
        tag_name: rule.tag_name.clone(),
        mode: match rule.mode {
            ai::TagRuleMode::Auto => ProtoTagRuleMode::Auto as i32,
            ai::TagRuleMode::Suggest => ProtoTagRuleMode::Suggest as i32,
        },
        min_conf: rule.min_conf,
        enabled: rule.enabled,
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
