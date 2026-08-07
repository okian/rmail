//! The `SavedSearchService` gRPC implementation (task 35).
//!
//! Mostly a thin translation over [`rmail_core::saved_search::SavedSearchStore`]
//! and [`rmail_core::smart_folder::SmartFolderStore`]. Two decisions here are
//! not thin, and both exist to keep this service from becoming a second
//! answer to a question something else already answers:
//!
//! # `RunSavedSearch` calls the real search path, it does not reimplement one
//!
//! prd.md: a saved search is "a named query string; re-run through the full
//! pipeline on demand." This handler resolves the name to its stored query
//! and hands that string to [`crate::search_service::SearchApi`]'s own
//! streaming entry point — the identical call `SearchService.Search` makes,
//! so a saved search is literally the same code path as typing the same
//! string. There is no stored result set anywhere to go stale (see
//! `rmail_core::saved_search`'s own module docs), and no second ranker for
//! the two to disagree about.
//!
//! `rmaild/tests/saved_search_service.rs` proves the equivalence directly:
//! `RunSavedSearch("weekly")` and `Search(<that query>)` return the same
//! hits in the same order.
//!
//! The one thing it deliberately does *not* inherit is the query-generation
//! slot. `Search`/`Semantic` pass `Generation::begin`'s token, which cancels
//! whichever stream held the slot before — correct for an interactive search
//! box, where the newer keystroke wins and the older scan is by construction
//! stale. A saved search is not a keystroke: it is a named query whose whole
//! promise is that running it returns what the query matches. Sharing the
//! slot would let a `RunSavedSearch` silently truncate a concurrent `Search`
//! (and the reverse, and two saved searches each other) — and a cancelled
//! pipeline stream simply *ends*, so the client would see a clean `OK` over a
//! short page rather than an error. `RunSavedSearch` therefore passes a plain
//! child of the shutdown token: still stopped by daemon shutdown, by nothing
//! else.
//!
//! # `ListSmartFolderMembers` streams a *view*, and writes nothing
//!
//! Membership is recomputed from the predicate on every call — see
//! `rmail_core::smart_folder`'s module docs. That is why this RPC is a plain
//! read that neither evaluates nor fires actions:
//! [`SavedSearchApi::evaluate_smart_folder`] is the separate, explicit call
//! for "re-evaluate and fire what is genuinely new", and the background
//! [`rmail_core::smart_folder::SmartFolderEvaluator`] is what normally makes
//! even that unnecessary.
#![allow(clippy::result_large_err)] // see mail_service.rs's note on `Result<_, Status>`

use std::pin::Pin;

use rmail_core::saved_search::{SavedSearch as CoreSavedSearch, SavedSearchStore};
use rmail_core::smart_folder::{
    Evaluation, NewSmartFolder, SmartFolder as CoreSmartFolder, SmartFolderStore,
};
use rmail_core::{repo, Database, Error as RmailError};
use rmail_proto::v1::saved_search_service_server::SavedSearchService;
use rmail_proto::v1::{
    CreateSavedSearchRequest, CreateSmartFolderRequest, DeleteSavedSearchRequest,
    DeleteSmartFolderRequest, EvaluateSmartFolderRequest, ListSavedSearchesRequest,
    ListSavedSearchesResponse, ListSmartFolderMembersRequest, ListSmartFoldersRequest,
    ListSmartFoldersResponse, Message as ProtoMessage, RunSavedSearchRequest,
    SavedSearch as ProtoSavedSearch, SearchHit as ProtoSearchHit, SearchRequest,
    SmartFolder as ProtoSmartFolder, SmartFolderEvaluation, UpdateSavedSearchRequest,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use crate::search_service::{to_proto_message, SearchApi};

/// How many members may sit between the predicate scan and a client before
/// `ListSmartFolderMembers` applies backpressure. See
/// `mail_service::STREAM_BUFFER` for the identical reasoning.
const STREAM_BUFFER: usize = 64;

/// How many ids one `SmartFolderEvaluation` reports per delta list.
///
/// A delta is unbounded in principle — a boot pass after a week of downtime,
/// or one "mark all read" against an `is:unread` folder, moves every member
/// at once — and this is a unary response, so the whole list would have to be
/// buffered into a single message. The counts stay exact; the lists are a
/// sample a UI can highlight, and full membership is what the *streaming*
/// `ListSmartFolderMembers` is for.
const MAX_REPORTED_IDS: usize = 256;

/// The `SavedSearchService` handler.
///
/// Cheap to clone: every field is a handle over the same database.
#[derive(Clone)]
pub struct SavedSearchApi {
    db: Database,
    searches: SavedSearchStore,
    folders: SmartFolderStore,
    /// The one search pipeline in the process — see the module docs.
    search: SearchApi,
    /// Cancelled at daemon shutdown, so an open member stream stops with it.
    shutdown: CancellationToken,
}

impl SavedSearchApi {
    /// Build the handler.
    #[must_use]
    pub fn new(
        db: Database,
        searches: SavedSearchStore,
        folders: SmartFolderStore,
        search: SearchApi,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            db,
            searches,
            folders,
            search,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl SavedSearchService for SavedSearchApi {
    #[tracing::instrument(skip(self, request), err)]
    async fn create_saved_search(
        &self,
        request: Request<CreateSavedSearchRequest>,
    ) -> Result<Response<ProtoSavedSearch>, Status> {
        let req = request.into_inner();
        let saved = self
            .searches
            .create(req.account_id, &req.name, &req.query)
            .await?;
        Ok(Response::new(saved_search_to_proto(&saved)))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn update_saved_search(
        &self,
        request: Request<UpdateSavedSearchRequest>,
    ) -> Result<Response<ProtoSavedSearch>, Status> {
        let req = request.into_inner();
        let saved = self
            .searches
            .update_query(req.account_id, &req.name, &req.query)
            .await?;
        Ok(Response::new(saved_search_to_proto(&saved)))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn list_saved_searches(
        &self,
        request: Request<ListSavedSearchesRequest>,
    ) -> Result<Response<ListSavedSearchesResponse>, Status> {
        let searches = self.searches.list(request.into_inner().account_id).await?;
        Ok(Response::new(ListSavedSearchesResponse {
            searches: searches.iter().map(saved_search_to_proto).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn delete_saved_search(
        &self,
        request: Request<DeleteSavedSearchRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.searches.delete(req.account_id, &req.name).await?;
        Ok(Response::new(()))
    }

    type RunSavedSearchStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request), err)]
    async fn run_saved_search(
        &self,
        request: Request<RunSavedSearchRequest>,
    ) -> Result<Response<Self::RunSavedSearchStream>, Status> {
        let req = request.into_inner();
        // Resolving and stamping `last_run_at` is one statement, so the query
        // that runs is exactly the one recorded as having run — see
        // `SavedSearchStore::resolve_for_run`.
        let saved = self
            .searches
            .resolve_for_run(req.account_id, &req.name)
            .await?;
        // The same entry point `SearchService.Search` uses. Everything a
        // search request carries that a saved search has no opinion about
        // (mode, intent, thread collapse) is left at its server default,
        // exactly as an unset field on a direct `Search` would be.
        self.search
            .start_stream(
                SearchRequest {
                    query: saved.query,
                    account_id: saved.account_id,
                    limit: req.limit,
                    explain: req.explain,
                    ..SearchRequest::default()
                },
                false,
                // Not the generation slot — see the module docs.
                self.shutdown.child_token(),
            )
            .await
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn create_smart_folder(
        &self,
        request: Request<CreateSmartFolderRequest>,
    ) -> Result<Response<ProtoSmartFolder>, Status> {
        let req = request.into_inner();
        let folder = self
            .folders
            .create(&NewSmartFolder {
                account_id: req.account_id,
                name: req.name,
                predicate: req.predicate,
                // Proto3 has no "absent string"; empty is the wire spelling
                // of "no auto-tag", and the core API's `Option` is the
                // in-process one.
                auto_tag: Some(req.auto_tag).filter(|tag| !tag.trim().is_empty()),
                notify: req.notify,
            })
            .await?;
        Ok(Response::new(smart_folder_to_proto(&folder)))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn list_smart_folders(
        &self,
        request: Request<ListSmartFoldersRequest>,
    ) -> Result<Response<ListSmartFoldersResponse>, Status> {
        let folders = self.folders.list(request.into_inner().account_id).await?;
        Ok(Response::new(ListSmartFoldersResponse {
            folders: folders.iter().map(smart_folder_to_proto).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn delete_smart_folder(
        &self,
        request: Request<DeleteSmartFolderRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.folders.delete(req.account_id, &req.name).await?;
        Ok(Response::new(()))
    }

    type ListSmartFolderMembersStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoMessage, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request), err)]
    async fn list_smart_folder_members(
        &self,
        request: Request<ListSmartFolderMembersRequest>,
    ) -> Result<Response<Self::ListSmartFolderMembersStream>, Status> {
        let req = request.into_inner();
        let cancel = self.shutdown.child_token();
        let folder = self.folders.get(req.account_id, &req.name).await?;
        // Resolved before the stream is handed back, so a predicate that
        // cannot run at all fails the call rather than a stream that opens
        // and then errors — the same shape `MailService.List` uses.
        // The bound goes into the scan, not onto its result — see
        // `SmartFolderStore::members`.
        let limit = (req.limit > 0).then_some(req.limit as usize);
        let members = self.folders.members(folder.id, limit, &cancel).await?;

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        let db = self.db.clone();
        tokio::spawn(
            async move {
                for id in members {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let item = match fetch_message(&db, id).await {
                        // A member that vanished between the scan and the
                        // fetch is skipped, not an error: membership is a
                        // live view, and "it is no longer there" is a
                        // correct answer to a question asked a moment ago.
                        Ok(None) => continue,
                        Ok(Some(message)) => Ok(message),
                        Err(error) => Err(Status::from(error)),
                    };
                    let failed = item.is_err();
                    if tx.send(item).await.is_err() || failed {
                        return;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    #[tracing::instrument(skip(self, request), err)]
    async fn evaluate_smart_folder(
        &self,
        request: Request<EvaluateSmartFolderRequest>,
    ) -> Result<Response<SmartFolderEvaluation>, Status> {
        let req = request.into_inner();
        let folder = self.folders.get(req.account_id, &req.name).await?;
        let evaluation = self
            .folders
            .evaluate(folder.id, &self.shutdown.child_token())
            .await?;
        Ok(Response::new(evaluation_to_proto(&evaluation)))
    }
}

/// Read one message plus its flags, or `None` if it is gone.
///
/// One pooled read, not two: a member stream is N of these, and paying two
/// connection acquisitions per row doubles the round trips for data that
/// comes off the same connection anyway.
async fn fetch_message(db: &Database, id: i64) -> Result<Option<ProtoMessage>, RmailError> {
    Ok(db
        .read(move |conn| {
            let Some(message) = repo::get_message(conn, id)? else {
                return Ok(None);
            };
            let flags = repo::list_flags(conn, id)?;
            Ok(Some((message, flags)))
        })
        .await?
        .map(|(message, flags)| to_proto_message(&message, flags)))
}

fn saved_search_to_proto(saved: &CoreSavedSearch) -> ProtoSavedSearch {
    ProtoSavedSearch {
        id: saved.id,
        account_id: saved.account_id,
        name: saved.name.clone(),
        query: saved.query.clone(),
        created_at: saved.created_at,
        updated_at: saved.updated_at,
        // Proto3 has no optional int64 here; 0 is the wire spelling of
        // "never run", and `saved_searches.last_run_at` never stores 0 (it
        // is written by `unixepoch()`).
        last_run_at: saved.last_run_at.unwrap_or(0),
    }
}

fn smart_folder_to_proto(folder: &CoreSmartFolder) -> ProtoSmartFolder {
    ProtoSmartFolder {
        id: folder.id,
        account_id: folder.account_id,
        name: folder.name.clone(),
        predicate: folder.predicate.clone(),
        auto_tag: folder.auto_tag.clone().unwrap_or_default(),
        notify: folder.notify,
        created_at: folder.created_at,
        updated_at: folder.updated_at,
        last_evaluated_at: folder.last_evaluated_at.unwrap_or(0),
    }
}

fn evaluation_to_proto(evaluation: &Evaluation) -> SmartFolderEvaluation {
    SmartFolderEvaluation {
        smart_folder_id: evaluation.smart_folder_id,
        members: u32::try_from(evaluation.members).unwrap_or(u32::MAX),
        entered: capped(&evaluation.entered),
        departed: capped(&evaluation.departed),
        tagged: u32::try_from(evaluation.tagged).unwrap_or(u32::MAX),
        notified: u32::try_from(evaluation.notified).unwrap_or(u32::MAX),
        entered_count: u32::try_from(evaluation.entered.len()).unwrap_or(u32::MAX),
        departed_count: u32::try_from(evaluation.departed.len()).unwrap_or(u32::MAX),
    }
}

/// The first [`MAX_REPORTED_IDS`] of a delta list — see the proto's own
/// comment on `SmartFolderEvaluation` for why a unary response cannot carry
/// the whole thing.
fn capped(ids: &[i64]) -> Vec<i64> {
    ids.iter().take(MAX_REPORTED_IDS).copied().collect()
}
