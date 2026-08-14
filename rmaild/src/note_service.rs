//! The `NoteService` gRPC implementation.
//!
//! A thin translation over [`rmail_core::notes::NoteStore`] — the store owns
//! the CRUD, the schema-enforced message-or-thread XOR (see its module
//! docs), the last-write-wins `EditNote` semantics, and the lexical-index
//! feed. This file's own design lives in [`NoteApi::watch_notes`]:
//!
//! # `WatchNotes` is a live tail, not a durable log
//!
//! Unlike `MailService.WatchEvents`/`SyncService.WatchEvents` (backed by
//! [`rmail_core::events::EventLog`], replay-then-follow with a resumable
//! cursor), `NoteStore::watch` has no durable backlog — see that method's
//! own docs for why prd.md's "refreshes open UIs" framing does not need one.
//! A subscriber that disconnects and reconnects gets the live tail from that
//! moment forward; it recovers whatever it missed with a fresh `ListNotes`,
//! the same as opening the view for the first time. There is accordingly no
//! `since_seq`/resume-gap handling here at all — the simplest correct thing
//! for a stream with no cursor to resume from.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::idempotency::IdempotencyStore;
use rmail_core::notes::{
    NewNote, Note as CoreNote, NoteAuthor as CoreAuthor, NoteChange, NoteStore,
    Target as CoreTarget,
};
use rmail_core::Error;
use rmail_proto::v1::note_service_server::NoteService;
use rmail_proto::v1::note_target::Of;
use rmail_proto::v1::{
    note_event, AddNoteRequest, DeleteNoteRequest, DeletedNote as ProtoDeletedNote,
    EditNoteRequest, ListNotesRequest, ListNotesResponse, Note as ProtoNote,
    NoteAuthor as ProtoAuthor, NoteEvent, NoteTarget as ProtoTarget, WatchNotesRequest,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many live [`NoteChange`]s may sit between [`NoteStore::watch`] and a
/// client before `WatchNotes` applies backpressure. See
/// `rmaild::mail_service::STREAM_BUFFER` — the same reasoning, duplicated
/// because the two streams are independent.
const STREAM_BUFFER: usize = 64;

/// The `NoteService` handler.
#[derive(Clone)]
pub struct NoteApi {
    store: NoteStore,
    /// Cancelled when the daemon shuts down, so an open `WatchNotes` stream
    /// stops with it rather than holding shutdown open.
    shutdown: CancellationToken,
    /// The replay fence behind `AddNote`'s `idempotency_key`. `notes` carries
    /// no uniqueness of its own — several notes on one message is a feature —
    /// so a retry is otherwise indistinguishable from a second note.
    idempotency: IdempotencyStore,
}

impl NoteApi {
    /// Build a handler over a note store.
    #[must_use]
    pub fn new(
        store: NoteStore,
        shutdown: CancellationToken,
        idempotency: IdempotencyStore,
    ) -> Self {
        Self {
            store,
            shutdown,
            idempotency,
        }
    }
}

const ADD_NOTE_METHOD: &str = "/rmail.v1.NoteService/AddNote";

#[tonic::async_trait]
impl NoteService for NoteApi {
    async fn add_note(
        &self,
        request: Request<AddNoteRequest>,
    ) -> Result<Response<ProtoNote>, Status> {
        let req = request.into_inner();
        let target = target_from_proto(req.target)?;
        let author = author_from_proto(req.author());
        let body_md = req.body_md.clone();
        crate::idempotency::guard(
            &self.idempotency,
            ADD_NOTE_METHOD,
            &req.idempotency_key,
            &req,
            async {
                let note = self
                    .store
                    .add(NewNote {
                        target,
                        body_md,
                        author,
                    })
                    .await?;
                Ok(note_to_proto(&note))
            },
        )
        .await
        .map(Response::new)
    }

    async fn edit_note(
        &self,
        request: Request<EditNoteRequest>,
    ) -> Result<Response<ProtoNote>, Status> {
        let req = request.into_inner();
        let note = self.store.edit(req.note_id, req.body_md).await?;
        Ok(Response::new(note_to_proto(&note)))
    }

    async fn delete_note(
        &self,
        request: Request<DeleteNoteRequest>,
    ) -> Result<Response<()>, Status> {
        let note_id = request.into_inner().note_id;
        self.store.delete(note_id).await?;
        Ok(Response::new(()))
    }

    async fn list_notes(
        &self,
        request: Request<ListNotesRequest>,
    ) -> Result<Response<ListNotesResponse>, Status> {
        let target = target_from_proto(request.into_inner().target)?;
        let notes = self.store.list(target).await?;
        Ok(Response::new(ListNotesResponse {
            notes: notes.iter().map(note_to_proto).collect(),
        }))
    }

    type WatchNotesStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<NoteEvent, Status>> + Send + 'static>>;

    async fn watch_notes(
        &self,
        request: Request<WatchNotesRequest>,
    ) -> Result<Response<Self::WatchNotesStream>, Status> {
        let cancel = self.shutdown.child_token();
        let req = request.into_inner();
        let filter = req.target.map(|t| target_from_proto(Some(t))).transpose()?;

        let mut changes = self.store.watch();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                loop {
                    let received = tokio::select! {
                        () = cancel.cancelled() => {
                            crate::stream::terminate_cancelled(&tx).await;
                            return;
                        }
                        received = changes.recv() => received,
                    };
                    match received {
                        Ok(change) => {
                            if filter.is_some_and(|target| change.target() != target) {
                                continue;
                            }
                            if send(&tx, &cancel, Ok(change_to_proto(change)))
                                .await
                                .is_break()
                            {
                                return;
                            }
                        }
                        // No durable backlog to recover from (see this
                        // module's own docs) — lagging just means resuming
                        // the live tail from here, the same as any other
                        // reconnect.
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::debug!(
                                missed,
                                "note change stream lagged; resuming the live tail"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// Send one stream item, giving up if the client went away or the daemon is
/// stopping. See `rmaild::mail_service::send` — identical reasoning.
async fn send(
    tx: &tokio::sync::mpsc::Sender<Result<NoteEvent, Status>>,
    cancel: &CancellationToken,
    item: Result<NoteEvent, Status>,
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

fn target_from_proto(target: Option<ProtoTarget>) -> Result<CoreTarget, Status> {
    let target =
        target.ok_or_else(|| Status::from(Error::invalid_argument("target is required")))?;
    match target.of {
        Some(Of::MessageId(id)) => Ok(CoreTarget::Message(id)),
        Some(Of::ThreadId(id)) => Ok(CoreTarget::Thread(id)),
        None => Err(Status::from(Error::invalid_argument(
            "target must set either message_id or thread_id",
        ))),
    }
}

fn target_to_proto(target: CoreTarget) -> ProtoTarget {
    ProtoTarget {
        of: Some(match target {
            CoreTarget::Message(id) => Of::MessageId(id),
            CoreTarget::Thread(id) => Of::ThreadId(id),
        }),
    }
}

fn author_from_proto(author: ProtoAuthor) -> CoreAuthor {
    match author {
        ProtoAuthor::Ai => CoreAuthor::Ai,
        // Unset defaults to `user` — see `AddNoteRequest.author`'s own doc
        // comment.
        ProtoAuthor::User | ProtoAuthor::Unspecified => CoreAuthor::User,
    }
}

fn author_to_proto(author: CoreAuthor) -> ProtoAuthor {
    match author {
        CoreAuthor::User => ProtoAuthor::User,
        CoreAuthor::Ai => ProtoAuthor::Ai,
    }
}

fn note_to_proto(note: &CoreNote) -> ProtoNote {
    ProtoNote {
        id: note.id,
        target: Some(target_to_proto(note.target)),
        body_md: note.body_md.clone(),
        author: author_to_proto(note.author) as i32,
        created_at: note.created_at,
        updated_at: note.updated_at,
    }
}

fn change_to_proto(change: NoteChange) -> NoteEvent {
    let event = match change {
        NoteChange::Added(note) => note_event::Event::Added(note_to_proto(&note)),
        NoteChange::Edited(note) => note_event::Event::Edited(note_to_proto(&note)),
        NoteChange::Deleted { id, target } => note_event::Event::Deleted(ProtoDeletedNote {
            id,
            target: Some(target_to_proto(target)),
        }),
    };
    NoteEvent { event: Some(event) }
}
