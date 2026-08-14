//! Integration test: drive `NoteService` end-to-end against an in-process
//! tonic server over a Unix domain socket, backed by a real
//! `rmail_core::notes::NoteStore` over a real (temp-file) database — the
//! same "build the handler directly, no auth layer, no fake transport"
//! discipline `ai_service.rs`'s own harness uses (see that file's module
//! docs for why: this crate has no in-process way to dial a real Anthropic
//! endpoint, and `NoteService` similarly has nothing worth faking here —
//! every dependency is already local).
//!
//! Covers the acceptance bullets `tasks.md` names for task 56 at the gRPC
//! boundary: Add/Edit/Delete/List/WatchNotes round-trip, the message-or-
//! thread XOR surfacing as `INVALID_ARGUMENT`/`NOT_FOUND` over the wire (not
//! just inside `rmail-core::notes`' own tests), last-write-wins on
//! `updated_at`, and markdown stored verbatim.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmail_core::index::{IndexQueue, QueueOptions as IndexQueueOptions};
use rmail_core::notes::NoteStore;
use rmail_core::repo;
use rmail_core::Database;
use rmail_proto::v1::note_service_client::NoteServiceClient;
use rmail_proto::v1::note_target::Of;
use rmail_proto::v1::{
    AddNoteRequest, DeleteNoteRequest, EditNoteRequest, ListNotesRequest, NoteAuthor, NoteTarget,
    WatchNotesRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// How long a stream assertion waits before failing — generous, since these
/// are liveness checks on a spawned task, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::sync::atomic::AtomicI64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_indexing(true).await
    }

    async fn start_with_indexing(index_enabled: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-note-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-note-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();

        let index_queue = IndexQueue::new(db.clone(), IndexQueueOptions::default());
        let store = NoteStore::new(db.clone(), index_queue, index_enabled);
        let shutdown_cancel = CancellationToken::new();
        let api = rmaild::NoteApi::new(
            store,
            shutdown_cancel.clone(),
            rmail_core::idempotency::IdempotencyStore::new(
                db.clone(),
                std::time::Duration::from_secs(3600),
                std::time::Duration::from_secs(60),
            ),
        );

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(rmail_proto::v1::note_service_server::NoteServiceServer::new(api))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                    shutdown_cancel.cancel();
                })
                .await;
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            db,
            account_id,
            mailbox_id,
            next_uid: std::sync::atomic::AtomicI64::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> NoteServiceClient<Channel> {
        NoteServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn message(&self) -> i64 {
        self.message_in_thread(None).await
    }

    async fn message_in_thread(&self, thread_id: Option<i64>) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        thread_id,
                        subject: Some("hi".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

fn message_target(id: i64) -> NoteTarget {
    NoteTarget {
        of: Some(Of::MessageId(id)),
    }
}

fn thread_target(id: i64) -> NoteTarget {
    NoteTarget {
        of: Some(Of::ThreadId(id)),
    }
}

#[tokio::test]
async fn add_edit_delete_and_list_round_trip_over_grpc() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let message_id = server.message().await;

    let markdown = "# heading\n\n- one\n- two\n\nsome *emphasis*";
    let note = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(message_id)),
            body_md: markdown.to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .expect("AddNote should succeed")
        .into_inner();
    assert_eq!(note.body_md, markdown, "markdown is stored verbatim");
    assert_eq!(note.author, NoteAuthor::User as i32);
    assert_eq!(
        note.target,
        Some(message_target(message_id)),
        "the target round-trips through the RPC"
    );

    let listed = client
        .list_notes(ListNotesRequest {
            target: Some(message_target(message_id)),
        })
        .await
        .expect("ListNotes should succeed")
        .into_inner();
    assert_eq!(listed.notes.len(), 1);
    assert_eq!(listed.notes[0].id, note.id);

    let edited = client
        .edit_note(EditNoteRequest {
            note_id: note.id,
            body_md: "revised body".to_owned(),
        })
        .await
        .expect("EditNote should succeed")
        .into_inner();
    assert_eq!(edited.body_md, "revised body");
    assert!(edited.updated_at >= note.updated_at);

    client
        .delete_note(DeleteNoteRequest { note_id: note.id })
        .await
        .expect("DeleteNote should succeed");

    let listed = client
        .list_notes(ListNotesRequest {
            target: Some(message_target(message_id)),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        listed.notes.is_empty(),
        "the deleted note must not still be listed"
    );

    server.stop().await;
}

#[tokio::test]
async fn add_and_list_round_trip_on_a_thread_target() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let thread_id = server.thread().await;

    let note = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(thread_target(thread_id)),
            body_md: "thread-wide context".to_owned(),
            author: NoteAuthor::Ai as i32,
        })
        .await
        .expect("AddNote against a thread should succeed")
        .into_inner();
    assert_eq!(note.target, Some(thread_target(thread_id)));
    assert_eq!(note.author, NoteAuthor::Ai as i32);

    let listed = client
        .list_notes(ListNotesRequest {
            target: Some(thread_target(thread_id)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.notes.len(), 1);
    assert_eq!(listed.notes[0].id, note.id);

    server.stop().await;
}

#[tokio::test]
async fn an_unset_target_is_rejected_as_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: None,
            body_md: "orphan".to_owned(),
            author: NoteAuthor::Unspecified as i32,
        })
        .await
        .expect_err("a request with no target must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn a_target_naming_a_message_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(999_999)),
            body_md: "orphan".to_owned(),
            author: NoteAuthor::Unspecified as i32,
        })
        .await
        .expect_err("a bogus message id must be rejected");
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

#[tokio::test]
async fn editing_or_deleting_a_note_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let edit_status = client
        .edit_note(EditNoteRequest {
            note_id: 999_999,
            body_md: "x".to_owned(),
        })
        .await
        .expect_err("editing a missing note must fail");
    assert_eq!(edit_status.code(), Code::NotFound);

    let delete_status = client
        .delete_note(DeleteNoteRequest { note_id: 999_999 })
        .await
        .expect_err("deleting a missing note must fail");
    assert_eq!(delete_status.code(), Code::NotFound);

    server.stop().await;
}

#[tokio::test]
async fn concurrent_edits_over_grpc_are_last_write_wins() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let message_id = server.message().await;

    let note = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(message_id)),
            body_md: "v1".to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .unwrap()
        .into_inner();

    // Two independent `EditNote` calls, neither carrying any version/ETag —
    // the request shape has none to carry — so both succeed and the second
    // to commit is simply what a subsequent read sees.
    client
        .edit_note(EditNoteRequest {
            note_id: note.id,
            body_md: "from editor A".to_owned(),
        })
        .await
        .unwrap();
    client
        .edit_note(EditNoteRequest {
            note_id: note.id,
            body_md: "from editor B".to_owned(),
        })
        .await
        .unwrap();

    let listed = client
        .list_notes(ListNotesRequest {
            target: Some(message_target(message_id)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.notes.len(), 1);
    assert_eq!(
        listed.notes[0].body_md, "from editor B",
        "the most recently committed edit wins outright"
    );

    server.stop().await;
}

#[tokio::test]
async fn watch_notes_streams_add_edit_and_delete_live() {
    let server = TestServer::start().await;
    let mut writer = server.client().await;
    let mut watcher = server.client().await;
    let message_id = server.message().await;

    let mut stream = watcher
        .watch_notes(WatchNotesRequest {
            target: Some(message_target(message_id)),
        })
        .await
        .expect("WatchNotes should open a stream")
        .into_inner();

    let note = writer
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(message_id)),
            body_md: "first".to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .unwrap()
        .into_inner();

    let added = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for the Added event")
        .expect("stream ended before Added")
        .expect("Added event should not be an error");
    match added.event {
        Some(rmail_proto::v1::note_event::Event::Added(got)) => assert_eq!(got.id, note.id),
        other => panic!("expected Added, got {other:?}"),
    }

    writer
        .edit_note(EditNoteRequest {
            note_id: note.id,
            body_md: "second".to_owned(),
        })
        .await
        .unwrap();
    let edited = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for the Edited event")
        .expect("stream ended before Edited")
        .expect("Edited event should not be an error");
    match edited.event {
        Some(rmail_proto::v1::note_event::Event::Edited(got)) => {
            assert_eq!(got.body_md, "second");
        }
        other => panic!("expected Edited, got {other:?}"),
    }

    writer
        .delete_note(DeleteNoteRequest { note_id: note.id })
        .await
        .unwrap();
    let deleted = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for the Deleted event")
        .expect("stream ended before Deleted")
        .expect("Deleted event should not be an error");
    match deleted.event {
        Some(rmail_proto::v1::note_event::Event::Deleted(got)) => {
            assert_eq!(got.id, note.id);
            assert_eq!(got.target, Some(message_target(message_id)));
        }
        other => panic!("expected Deleted, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test]
async fn watch_notes_filters_out_changes_for_a_different_target() {
    let server = TestServer::start().await;
    let mut writer = server.client().await;
    let mut watcher = server.client().await;
    let watched = server.message().await;
    let other = server.message().await;

    let mut stream = watcher
        .watch_notes(WatchNotesRequest {
            target: Some(message_target(watched)),
        })
        .await
        .unwrap()
        .into_inner();

    // A change on a different target must not appear on this subscription.
    writer
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(other)),
            body_md: "not for you".to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .unwrap();
    // Then a change on the watched target, which must appear.
    let note = writer
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(watched)),
            body_md: "for you".to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .unwrap()
        .into_inner();

    let event = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for an event")
        .expect("stream ended unexpectedly")
        .expect("event should not be an error");
    match event.event {
        Some(rmail_proto::v1::note_event::Event::Added(got)) => assert_eq!(got.id, note.id),
        other => panic!("expected Added for the watched target, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test]
async fn indexing_disabled_still_serves_the_grpc_surface_normally() {
    // `config.notes.index = false` only turns off the lexical-index feed
    // (proven at the `NoteStore` layer in `rmail-core::notes::tests`) --
    // the gRPC CRUD surface itself must behave identically either way.
    let server = TestServer::start_with_indexing(false).await;
    let mut client = server.client().await;
    let message_id = server.message().await;

    let note = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(message_target(message_id)),
            body_md: "still works".to_owned(),
            author: NoteAuthor::User as i32,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(note.body_md, "still works");

    server.stop().await;
}

/// A retried `AddNote` under the same key leaves one note, not two.
///
/// `notes` has no uniqueness of its own — several notes on one message is a
/// feature, not an accident — so nothing below the fence can tell a retry from
/// a second note the user genuinely wrote. Task 40 built the fence and wired
/// it to the IMAP-facing mutations; this is the local-store half, where a
/// duplicate is silent and permanent rather than reconciled by the next sync.
#[tokio::test]
async fn a_retried_add_note_under_one_key_leaves_a_single_note() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let message_id = server.message().await;

    let request = AddNoteRequest {
        idempotency_key: "note-retry-1".to_owned(),
        target: Some(message_target(message_id)),
        body_md: "chase this on Friday".to_owned(),
        author: NoteAuthor::User as i32,
    };

    let first = client.add_note(request.clone()).await.unwrap().into_inner();
    // The replay returns the *same* note, byte for byte — not a new one that
    // merely looks alike.
    let replay = client.add_note(request.clone()).await.unwrap().into_inner();
    assert_eq!(
        replay.id, first.id,
        "the retry replayed the cached response"
    );

    let listed = client
        .list_notes(ListNotesRequest {
            target: Some(message_target(message_id)),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        listed.notes.len(),
        1,
        "a retry must not leave the user with two copies of one note"
    );

    // Same key, different body: the caller has changed the call under a key it
    // already used, which is a client bug the fence names rather than guesses at.
    let status = client
        .add_note(AddNoteRequest {
            body_md: "a different note entirely".to_owned(),
            ..request
        })
        .await
        .expect_err("a reused key with a changed payload is refused");
    assert_eq!(status.code(), tonic::Code::AlreadyExists);
}
