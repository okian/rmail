//! Integration test: drive `MailService` end-to-end against an in-process
//! tonic server over a Unix domain socket.
//!
//! `MailService`'s IMAP calls go through a fake [`ImapMutator`] rather than a
//! real connection — there is no live IMAP server to dial in-process, and
//! `rmail_core::imap::mutate`'s own unit tests already prove the real wire
//! commands against its in-crate mock (invisible from here — see that
//! module's docs). What this suite proves instead is everything specific to
//! the gRPC surface and the ordering contract `rmail_core::mail::MailStore`
//! promises: CRUD over the wire, threaded `Get`, attachment chunking within
//! the frame cap, `WatchEvents`' replay-then-follow contract, and that both
//! streaming RPCs release their work when a client disconnects or the daemon
//! shuts down.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmail_core::events::{EventKind as CoreEventKind, EventLog, NewEvent, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::Error;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::{
    CopyRequest, DeleteRequest, GetAttachmentRequest, GetMessageRequest, GetThreadRequest,
    ListMessagesRequest, MoveRequest, SetFlagsRequest, WatchEventsRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a stream assertion waits before failing. Generous because these
/// are liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// A fake IMAP mutator: records every call, and can be told to fail each kind
// on demand, so IMAP-reflection ordering is testable without a live server.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    SetFlags {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        flags: Vec<String>,
    },
    Move {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        dest: String,
    },
    Copy {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        dest: String,
    },
    Delete {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
    },
}

#[derive(Debug, Default)]
struct FakeImap {
    calls: Mutex<Vec<Call>>,
    fail_set_flags: bool,
    fail_move: bool,
    fail_copy: bool,
    fail_delete: bool,
}

impl FakeImap {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ImapMutator for FakeImap {
    async fn set_flags(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        flags: &[String],
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::SetFlags {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            flags: flags.to_vec(),
        });
        if self.fail_set_flags {
            return Err(Error::unavailable("fake imap: set_flags refused"));
        }
        Ok(())
    }

    async fn move_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Move {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            dest: dest.to_owned(),
        });
        if self.fail_move {
            return Err(Error::unavailable("fake imap: move refused"));
        }
        Ok(())
    }

    async fn copy_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Copy {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            dest: dest.to_owned(),
        });
        if self.fail_copy {
            return Err(Error::unavailable("fake imap: copy refused"));
        }
        Ok(())
    }

    async fn delete_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Delete {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
        });
        if self.fail_delete {
            return Err(Error::unavailable("fake imap: delete refused"));
        }
        Ok(())
    }

    // Not exercised by this suite (task 55's tag keyword round-trip has its
    // own gRPC test, `tag_service.rs`) — present only so this fake satisfies
    // the full `ImapMutator` trait.
    async fn store_keyword(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uids: &[i64],
        _keyword: &str,
        _prefer_gmail_label: bool,
        _add: bool,
    ) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    /// The *server's* log — see `sync_service.rs`'s harness for why a test
    /// must append through this instance, not a second `EventLog` over the
    /// same database, to reach an open stream.
    log: EventLog,
    imap: Arc<FakeImap>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::with_imap(FakeImap::default()).await
    }

    async fn with_imap(imap: FakeImap) -> Self {
        Self::with_config(Retention::unlimited(), imap).await
    }

    /// A server whose event log has a caller-chosen retention, so a test can
    /// provoke a resume gap (`OUT_OF_RANGE`) on purpose.
    async fn with_retention(retention: Retention) -> Self {
        Self::with_config(retention, FakeImap::default()).await
    }

    async fn with_config(retention: Retention, imap: FakeImap) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-mail-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-mail-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), retention);
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let imap = Arc::new(imap);
        let mail_store = MailStore::new(
            db.clone(),
            log.clone(),
            imap.clone() as Arc<dyn ImapMutator>,
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            // Semantic indexing off: this suite only exercises `MailService`,
            // and an enabled default would make every test here pay to load
            // (or, on a cold cache, download) an ONNX model purely because
            // `serve_uds_with_engine_and_mail_store` now also wires up
            // `SearchService` — see `rmaild::serve_uds`'s own identical
            // convention.
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            rmaild::serve_uds_with_engine_and_mail_store(
                &server_socket,
                server_db,
                engine,
                mail_store,
                &config,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
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
            log,
            imap,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> MailServiceClient<Channel> {
        MailServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    fn log(&self) -> EventLog {
        self.log.clone()
    }

    fn imap_calls(&self) -> Vec<Call> {
        self.imap.calls()
    }

    /// An account with an INBOX and an Archive mailbox, one message in INBOX
    /// with the given flags, and its raw bytes if given. Returns
    /// `(account_id, inbox_id, archive_id, message_id)`.
    fn seed(&self, flags: &[&str], raw: Option<Vec<u8>>) -> (i64, i64, i64, i64) {
        let account_id = self
            .db
            .with_write(|c| {
                repo::insert_account(
                    c,
                    &NewAccount {
                        name: format!("Personal-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let inbox_id = self
            .db
            .with_write(|c| {
                repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let archive_id = self
            .db
            .with_write(|c| {
                repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let has_attachments = raw.is_some();
        let message_id = self
            .db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id,
                        mailbox_id: inbox_id,
                        uid: 42,
                        uidvalidity: 1,
                        subject: Some("Hi".to_owned()),
                        raw,
                        has_attachments,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        for flag in flags {
            self.db
                .with_write(|c| repo::add_flag(c, message_id, flag))
                .unwrap();
        }
        (account_id, inbox_id, archive_id, message_id)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Minimal base64 encoder (test-only): nothing in this workspace exposes one
/// to a plain byte slice without a new dependency for a single test fixture,
/// and RFC822 attachments have to be base64 to embed arbitrary bytes.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A `multipart/mixed` RFC822 message with one text body and one attachment
/// carrying `bytes`, base64-encoded and wrapped at 76 columns like a real
/// mail client would.
fn message_with_attachment(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let encoded = base64_encode(bytes);
    let wrapped = encoded
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 alphabet is ASCII"))
        .collect::<Vec<_>>()
        .join("\r\n");
    format!(
        "From: a@example.com\r\n\
         Subject: Attachment\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\
         \r\n\
         --b\r\n\
         Content-Type: text/plain\r\n\
         \r\n\
         see attached\r\n\
         --b\r\n\
         Content-Type: {content_type}; name=\"{filename}\"\r\n\
         Content-Disposition: attachment; filename=\"{filename}\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {wrapped}\r\n\
         --b--\r\n"
    )
    .into_bytes()
}

/// Take the next stream item, failing rather than hanging.
async fn next<S, T>(stream: &mut S) -> T
where
    S: tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a stream item")
        .expect("stream ended early")
        .expect("stream item was an error")
}

// ---------------------------------------------------------------------------
// List / Get / GetThread
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_streams_a_mailboxs_messages_with_flags() {
    let server = TestServer::start().await;
    let (_account_id, inbox_id, _archive_id, message_id) = server.seed(&["\\Seen"], None);
    let mut client = server.client().await;

    let mut stream = client
        .list(ListMessagesRequest {
            mailbox_id: inbox_id,
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let message = next(&mut stream).await;
    assert_eq!(message.id, message_id);
    assert_eq!(message.subject.as_deref(), Some("Hi"));
    assert_eq!(message.flags, vec!["\\Seen".to_owned()]);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream should end, not hang")
            .is_none(),
        "only one message was seeded"
    );

    server.stop().await;
}

#[tokio::test]
async fn get_returns_the_full_message_with_attachments() {
    let server = TestServer::start().await;
    let raw = message_with_attachment("doc.txt", "text/plain", b"hello attachment");
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let message_id = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 43,
                    uidvalidity: 1,
                    raw: Some(raw),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    server
        .db
        .with_write(move |c| {
            repo::insert_attachment(
                c,
                &repo::NewAttachment {
                    message_id,
                    part_id: Some("0".to_owned()),
                    filename: Some("doc.txt".to_owned()),
                    content_type: Some("text/plain".to_owned()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let full = client
        .get(GetMessageRequest { id: message_id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(full.message.as_ref().unwrap().id, message_id);
    assert_eq!(full.attachments.len(), 1);
    assert_eq!(full.attachments[0].filename.as_deref(), Some("doc.txt"));

    let missing = client
        .get(GetMessageRequest { id: 999_999 })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);

    server.stop().await;
}

#[tokio::test]
async fn get_thread_returns_messages_oldest_first() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let thread_id = server
        .db
        .with_write(move |c| {
            repo::insert_thread(
                c,
                &repo::NewThread {
                    account_id,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let older = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 100,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    date: Some(1_000),
                    subject: Some("First".to_owned()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let newer = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 101,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    date: Some(2_000),
                    subject: Some("Second".to_owned()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let thread = client
        .get_thread(GetThreadRequest { id: thread_id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(thread.messages.len(), 2);
    assert_eq!(thread.messages[0].id, older, "oldest first");
    assert_eq!(thread.messages[1].id, newer);

    let missing = client
        .get_thread(GetThreadRequest { id: 999_999 })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Mutations: reflect to IMAP, then locally, then an event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_flags_reflects_to_imap_updates_locally_and_emits_an_event() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, _archive_id, message_id) = server.seed(&["\\Seen"], None);
    let mut client = server.client().await;

    client
        .set_flags(SetFlagsRequest {
            message_id,
            flags: vec!["\\Seen".to_owned(), "\\Flagged".to_owned()],
            idempotency_key: String::new(),
        })
        .await
        .unwrap();

    let stored = server
        .db
        .with_read(|c| repo::list_flags(c, message_id))
        .unwrap();
    assert_eq!(stored, vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]);
    assert_eq!(
        server.imap_calls(),
        vec![Call::SetFlags {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            flags: vec!["\\Seen".to_owned(), "\\Flagged".to_owned()],
        }]
    );
    let page = server.log().since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, CoreEventKind::FlagChanged);
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));

    server.stop().await;
}

#[tokio::test]
async fn set_flags_with_an_unsafe_flag_is_invalid_argument() {
    let server = TestServer::start().await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    let status = client
        .set_flags(SetFlagsRequest {
            message_id,
            flags: vec!["not a flag".to_owned()],
            idempotency_key: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        server.imap_calls().is_empty(),
        "must fail before calling IMAP"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_refused_imap_call_leaves_local_state_untouched() {
    let server = TestServer::with_imap(FakeImap {
        fail_set_flags: true,
        ..Default::default()
    })
    .await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&["\\Seen"], None);
    let mut client = server.client().await;

    let status = client
        .set_flags(SetFlagsRequest {
            message_id,
            flags: vec!["\\Flagged".to_owned()],
            idempotency_key: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);

    let stored = server
        .db
        .with_read(|c| repo::list_flags(c, message_id))
        .unwrap();
    assert_eq!(stored, vec!["\\Seen".to_owned()], "local flags untouched");
    let page = server.log().since(0, 10).await.unwrap();
    assert!(page.events.is_empty(), "a failed mutation emits no event");

    server.stop().await;
}

#[tokio::test]
async fn move_reflects_to_imap_drops_the_local_row_and_emits_a_moved_event() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    client
        .r#move(MoveRequest {
            message_id,
            dest_mailbox_id: archive_id,
            idempotency_key: String::new(),
        })
        .await
        .unwrap();

    assert!(
        server
            .db
            .with_read(|c| repo::get_message(c, message_id))
            .unwrap()
            .is_none(),
        "the local row is dropped, not re-pointed at a guessed identity"
    );
    assert_eq!(
        server.imap_calls(),
        vec![Call::Move {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            dest: "Archive".to_owned(),
        }]
    );
    let page = server.log().since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, CoreEventKind::Moved);
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));

    server.stop().await;
}

#[tokio::test]
async fn move_across_accounts_is_rejected_without_calling_imap() {
    let server = TestServer::start().await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let other_account = server
        .db
        .with_write(|c| {
            repo::insert_account(
                c,
                &NewAccount {
                    name: "Other".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let other_mailbox = server
        .db
        .with_write(move |c| {
            repo::insert_mailbox(
                c,
                &NewMailbox {
                    account_id: other_account,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let status = client
        .r#move(MoveRequest {
            message_id,
            dest_mailbox_id: other_mailbox,
            idempotency_key: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(server.imap_calls().is_empty());

    server.stop().await;
}

#[tokio::test]
async fn copy_reflects_to_imap_and_leaves_local_state_and_events_untouched() {
    let server = TestServer::start().await;
    let (account_id, _inbox_id, archive_id, message_id) = server.seed(&["\\Seen"], None);
    let mut client = server.client().await;

    client
        .copy(CopyRequest {
            message_id,
            dest_mailbox_id: archive_id,
            idempotency_key: String::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        server.imap_calls(),
        vec![Call::Copy {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            dest: "Archive".to_owned(),
        }]
    );
    assert!(server
        .db
        .with_read(|c| repo::get_message(c, message_id))
        .unwrap()
        .is_some());
    let page = server.log().since(0, 10).await.unwrap();
    assert!(page.events.is_empty(), "copy emits no local event");

    server.stop().await;
}

#[tokio::test]
async fn delete_reflects_to_imap_drops_the_local_row_and_emits_a_deleted_event() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    client
        .delete(DeleteRequest {
            message_id,
            idempotency_key: String::new(),
        })
        .await
        .unwrap();

    assert!(server
        .db
        .with_read(|c| repo::get_message(c, message_id))
        .unwrap()
        .is_none());
    assert_eq!(
        server.imap_calls(),
        vec![Call::Delete {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
        }]
    );
    let page = server.log().since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, CoreEventKind::Deleted);
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));

    server.stop().await;
}

#[tokio::test]
async fn deleting_an_unknown_message_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .delete(DeleteRequest {
            message_id: 999_999,
            idempotency_key: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// GetAttachment: chunking within the frame cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_attachment_streams_a_small_attachment_in_one_chunk() {
    let server = TestServer::start().await;
    let bytes = b"hello attachment".to_vec();
    let raw = message_with_attachment("doc.txt", "text/plain", &bytes);
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let message_id = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 44,
                    uidvalidity: 1,
                    raw: Some(raw),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let mut stream = client
        .get_attachment(GetAttachmentRequest {
            message_id,
            part_id: "0".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    let chunk = next(&mut stream).await;
    assert_eq!(chunk.filename.as_deref(), Some("doc.txt"));
    assert_eq!(chunk.content_type.as_deref(), Some("text/plain"));
    assert_eq!(chunk.total_size, Some(bytes.len() as i64));
    assert_eq!(chunk.data, bytes);
    assert!(tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should end, not hang")
        .is_none());

    server.stop().await;
}

#[tokio::test]
async fn get_attachment_for_an_unknown_part_is_not_found() {
    let server = TestServer::start().await;
    let raw = message_with_attachment("doc.txt", "text/plain", b"hi");
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let message_id = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 45,
                    uidvalidity: 1,
                    raw: Some(raw),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let status = client
        .get_attachment(GetAttachmentRequest {
            message_id,
            part_id: "99".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

#[tokio::test]
async fn attachment_larger_than_one_chunk_streams_correctly() {
    // 600,000 bytes over a 256 KiB (262,144-byte) chunk size is three chunks:
    // two full ones and a short last one — the seam that matters for proving
    // reassembly, not merely that streaming happens at all.
    let server = TestServer::start().await;
    let bytes: Vec<u8> = (0..600_000usize).map(|i| (i % 251) as u8).collect();
    let raw = message_with_attachment("big.bin", "application/octet-stream", &bytes);
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let message_id = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 46,
                    uidvalidity: 1,
                    raw: Some(raw),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let mut stream = client
        .get_attachment(GetAttachmentRequest {
            message_id,
            part_id: "0".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();

    let mut reassembled = Vec::with_capacity(bytes.len());
    let mut chunk_count = 0;
    let mut total_size = None;
    loop {
        let Some(item) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .expect("timed out waiting for a chunk")
        else {
            break;
        };
        let chunk = item.expect("chunk should not be an error");
        if chunk_count == 0 {
            assert_eq!(chunk.filename.as_deref(), Some("big.bin"));
            total_size = chunk.total_size;
        }
        reassembled.extend_from_slice(&chunk.data);
        chunk_count += 1;
    }

    assert!(
        chunk_count > 1,
        "600,000 bytes over a 256 KiB chunk size must span more than one frame"
    );
    assert_eq!(total_size, Some(bytes.len() as i64));
    assert_eq!(reassembled, bytes, "reassembled bytes must match exactly");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// WatchEvents: replay-then-follow, and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watch_events_replays_the_backlog_then_follows_the_tail() {
    let server = TestServer::start().await;
    let (account_id, _inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let log = server.log();
    log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
        .await
        .unwrap();
    let mut client = server.client().await;

    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let first = next(&mut stream).await;
    assert_eq!(first.seq, 1, "the backlog replays first");

    log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
        .await
        .unwrap();
    let second = next(&mut stream).await;
    assert_eq!(second.seq, 2, "then the live tail follows, with no gap");

    server.stop().await;
}

#[tokio::test]
async fn watch_events_rejects_a_negative_cursor() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .watch_events(WatchEventsRequest {
            account_id: 0,
            since_seq: -1,
            kinds: Vec::new(),
        })
        .await
        .expect_err("a negative cursor is not a valid resume point");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn watch_events_rejects_a_filter_of_only_unknown_kinds() {
    // Silently widening to "every kind" here would hand a narrowly-scoped
    // caller the firehose instead of telling it the filter made no sense.
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .watch_events(WatchEventsRequest {
            account_id: 0,
            since_seq: 0,
            kinds: vec![999],
        })
        .await
        .expect_err("no recognised kind in the filter must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn watch_events_filters_by_account_and_kind() {
    let server = TestServer::start().await;
    let (account_id, ..) = server.seed(&[], None);
    let (other_account, ..) = server.seed(&[], None);
    let log = server.log();
    // Admitted: right account, right kind.
    log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
        .await
        .unwrap();
    // Filtered out: right account, wrong kind.
    log.append(NewEvent::new(CoreEventKind::SyncState).account(account_id))
        .await
        .unwrap();
    // Filtered out: wrong account.
    log.append(NewEvent::new(CoreEventKind::NewMail).account(other_account))
        .await
        .unwrap();
    // Admitted: the second one this subscription should ever see.
    log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
        .await
        .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: vec![rmail_proto::v1::EventKind::NewMail as i32],
        })
        .await
        .unwrap()
        .into_inner();

    let first = next(&mut stream).await;
    assert_eq!(first.seq, 1);
    let second = next(&mut stream).await;
    assert_eq!(second.seq, 4, "seq 2 and 3 must both be filtered out");

    server.stop().await;
}

#[tokio::test]
async fn watch_events_rejects_a_cursor_past_retention_and_says_where_to_resume() {
    use tonic_types::StatusExt;

    let server = TestServer::with_retention(Retention {
        max_rows: Some(3),
        max_age: None,
    })
    .await;
    let (account_id, ..) = server.seed(&[], None);
    let log = server.log();
    for _ in 1..=10 {
        log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
            .await
            .unwrap();
    }
    log.prune().await.unwrap();

    let mut client = server.client().await;
    let status = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 1,
            kinds: Vec::new(),
        })
        .await
        .expect_err("cursor 1 was pruned away");

    assert_eq!(status.code(), Code::OutOfRange);
    let details = status.get_error_details();
    let info = details.error_info().expect("ErrorInfo attached");
    assert!(
        info.metadata
            .contains_key(rmail_core::error::OLDEST_SEQ_KEY),
        "the client is told how far back the log goes: {:?}",
        info.metadata
    );

    server.stop().await;
}

#[tokio::test]
async fn dropping_the_watch_stream_stops_the_producer() {
    let server = TestServer::start().await;
    let (account_id, _inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let log = server.log();
    log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
        .await
        .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(next(&mut stream).await.seq, 1);
    assert_eq!(log.subscriber_count(), 1, "the stream holds a subscription");

    drop(stream);
    drop(client);

    let mut released = false;
    for _ in 0..300 {
        log.append(NewEvent::new(CoreEventKind::NewMail).account(account_id))
            .await
            .unwrap();
        if log.subscriber_count() == 0 {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        released,
        "the producer task outlived the client that asked for it"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_shutdown_closes_an_open_watch_stream_rather_than_holding_it() {
    let server = TestServer::start().await;
    let (account_id, ..) = server.seed(&[], None);
    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    let shutdown = tokio::time::timeout(Duration::from_secs(30), server.stop());
    assert!(
        shutdown.await.is_ok(),
        "shutdown must not wait on an open event stream"
    );

    let ended = tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await;
    assert!(ended.is_ok(), "the stream should end when the server does");
}

// ---------------------------------------------------------------------------
// GetAttachment: cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_shutdown_closes_an_open_attachment_stream_rather_than_holding_it() {
    // A client that opened the stream but is not (yet) reading it — the
    // producer is left mid-flight, potentially blocked on the bounded
    // channel. If its cancellation token were not wired to the daemon's
    // shutdown, tonic's graceful shutdown would wait for this connection to
    // close, which it never would on its own here.
    let server = TestServer::start().await;
    let bytes: Vec<u8> = (0..2_000_000usize).map(|i| (i % 251) as u8).collect();
    let raw = message_with_attachment("big.bin", "application/octet-stream", &bytes);
    let (account_id, inbox_id, _archive_id, _message_id) = server.seed(&[], None);
    let message_id = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 47,
                    uidvalidity: 1,
                    raw: Some(raw),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mut client = server.client().await;

    let stream = client
        .get_attachment(GetAttachmentRequest {
            message_id,
            part_id: "0".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    // Deliberately never read: the 2 MB attachment is well over one chunk
    // buffer's worth, so the producer is almost certainly blocked on a full
    // channel by the time shutdown begins.
    let _ = stream;

    let shutdown = tokio::time::timeout(Duration::from_secs(30), server.stop());
    assert!(
        shutdown.await.is_ok(),
        "shutdown must not wait on an open (unread) attachment stream"
    );
}

// ---------------------------------------------------------------------------
// Pagination (task 40)
// ---------------------------------------------------------------------------

/// Seed `count` extra messages into `mailbox_id`, all on one timestamp — the
/// tie a page boundary has to survive. See `rmail_core::mail::tests`'
/// `seed_tied` for the same reasoning.
fn seed_tied(server: &TestServer, account_id: i64, mailbox_id: i64, count: i64) -> Vec<i64> {
    (0..count)
        .map(|n| {
            server
                .db
                .with_write(move |c| {
                    repo::insert_message(
                        c,
                        &NewMessage {
                            account_id,
                            mailbox_id,
                            uid: 5_000 + n,
                            uidvalidity: 1,
                            date: Some(1_700_000_000),
                            ..Default::default()
                        },
                    )
                })
                .unwrap()
        })
        .collect()
}

/// One page of `List`, plus the `x-rmail-next-page-token` header if there was
/// one. The header is where a *streamed* list has to carry its token — see
/// `rmaild::mail_service`'s module docs.
async fn list_page(
    client: &mut MailServiceClient<Channel>,
    mailbox_id: i64,
    page_size: i32,
    page_token: &str,
) -> (Vec<i64>, Option<String>) {
    let response = client
        .list(ListMessagesRequest {
            mailbox_id,
            page_size,
            page_token: page_token.to_owned(),
        })
        .await
        .unwrap();
    let next = response
        .metadata()
        .get(rmail_core::page::NEXT_PAGE_TOKEN_METADATA_KEY)
        .map(|value| value.to_str().unwrap().to_owned());
    let mut stream = response.into_inner();
    let mut ids = Vec::new();
    while let Some(message) = stream.next().await {
        ids.push(message.unwrap().id);
    }
    (ids, next)
}

#[tokio::test]
async fn a_shutdown_ends_an_open_watch_stream_with_cancelled_not_ok() {
    // The contract every streaming RPC now shares. `WatchEvents` is the
    // clearest case: it never completes on its own, so before this change a
    // shutdown produced a clean `OK` — indistinguishable, from the client's
    // side, from a feed that had simply run out of events. A watcher would
    // have concluded there was nothing more to see and stopped reconnecting.
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let mut stream = client
        .watch_events(WatchEventsRequest {
            since_seq: 0,
            account_id: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    // The producer is parked on the live tail with an empty backlog by the
    // time shutdown lands — the state the terminal frame has to survive.
    let db_path = server.db_path.clone();
    let socket = server.socket.clone();
    let _ = server.shutdown.send(());

    let mut terminal = None;
    while let Ok(Some(item)) = tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
        if let Err(status) = item {
            terminal = Some(status);
            break;
        }
    }
    let status = terminal.expect("a cancelled watch stream must not simply end");
    assert_eq!(status.code(), Code::Cancelled, "{status:?}");

    let _ = tokio::time::timeout(Duration::from_secs(10), server.handle).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn list_pages_through_a_mailbox_exactly_once() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, _archive_id, seeded) = server.seed(&[], None);
    let mut expected = vec![seeded];
    expected.extend(seed_tied(&server, account_id, inbox_id, 6));
    expected.sort_unstable();

    let mut client = server.client().await;
    let mut seen = Vec::new();
    let mut token = String::new();
    for _ in 0..10 {
        let (ids, next) = list_page(&mut client, inbox_id, 2, &token).await;
        assert!(ids.len() <= 2, "page over the requested size: {ids:?}");
        seen.extend(ids);
        match next {
            Some(next) => token = next,
            None => break,
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, expected, "paging repeated or skipped a message");

    server.stop().await;
}

#[tokio::test]
async fn the_final_list_page_carries_no_token_header() {
    let server = TestServer::start().await;
    let (account_id, inbox_id, _archive_id, _seeded) = server.seed(&[], None);
    seed_tied(&server, account_id, inbox_id, 3);
    let mut client = server.client().await;

    let (ids, next) = list_page(&mut client, inbox_id, 4, "").await;
    assert_eq!(ids.len(), 4);
    assert_eq!(
        next, None,
        "an exhausted list must say so; a token here costs every client an \
         extra empty round trip"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_negative_list_page_size_is_invalid_argument() {
    // The same answer ListDrafts/ListOutbox/ListFollowups give. A negative
    // page size is nonsense rather than a request for the default, and one
    // list RPC quietly disagreeing is the kind of inconsistency a client
    // only discovers in production.
    let server = TestServer::start().await;
    let (_account_id, inbox_id, _archive_id, _seeded) = server.seed(&[], None);
    let status = server
        .client()
        .await
        .list(ListMessagesRequest {
            mailbox_id: inbox_id,
            page_size: -1,
            page_token: String::new(),
        })
        .await
        .expect_err("a negative page size must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    server.stop().await;
}

#[tokio::test]
async fn a_list_page_token_cannot_be_re_aimed_at_another_mailbox() {
    // The token is caller-supplied input. Replayed against a mailbox it was
    // not minted for it must be refused, not honoured as a bare offset.
    let server = TestServer::start().await;
    let (account_id, inbox_id, archive_id, _seeded) = server.seed(&[], None);
    seed_tied(&server, account_id, inbox_id, 4);
    seed_tied(&server, account_id, archive_id, 4);
    let mut client = server.client().await;

    let (_ids, token) = list_page(&mut client, inbox_id, 2, "").await;
    let token = token.expect("a full page should carry a token");

    let status = client
        .list(ListMessagesRequest {
            mailbox_id: archive_id,
            page_size: 2,
            page_token: token,
        })
        .await
        .expect_err("a token from another mailbox must be refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Idempotency (task 40)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_retried_move_under_one_idempotency_key_reaches_imap_once() {
    let server = TestServer::start().await;
    let (_account_id, _inbox_id, archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    let request = MoveRequest {
        message_id,
        dest_mailbox_id: archive_id,
        idempotency_key: "move-key-1".to_owned(),
    };
    client.r#move(request.clone()).await.unwrap();
    // The retry a client makes when it never saw the first response. Without
    // the fence this reaches IMAP a second time — against a message the local
    // mirror has already dropped, so it is also a NOT_FOUND the caller cannot
    // interpret.
    client
        .r#move(request)
        .await
        .expect("a retry under the same key must replay, not fail");

    assert_eq!(
        server
            .imap_calls()
            .iter()
            .filter(|call| matches!(call, Call::Move { .. }))
            .count(),
        1,
        "the mailbox was mutated twice: {:?}",
        server.imap_calls()
    );

    server.stop().await;
}

#[tokio::test]
async fn reusing_an_idempotency_key_with_a_different_payload_is_already_exists() {
    let server = TestServer::start().await;
    let (_account_id, inbox_id, archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    client
        .r#move(MoveRequest {
            message_id,
            dest_mailbox_id: archive_id,
            idempotency_key: "shared-key".to_owned(),
        })
        .await
        .unwrap();

    let status = client
        .r#move(MoveRequest {
            message_id,
            dest_mailbox_id: inbox_id,
            idempotency_key: "shared-key".to_owned(),
        })
        .await
        .expect_err("a key names one call; a changed payload must not replay it");
    assert_eq!(status.code(), Code::AlreadyExists, "{status:?}");

    server.stop().await;
}

#[tokio::test]
async fn a_failed_mutation_releases_its_idempotency_key() {
    // A transient IMAP outage must not poison the key: the mutation did not
    // apply (IMAP is called before any local write), so the retry is the
    // first attempt again.
    let server = TestServer::with_imap(FakeImap {
        fail_set_flags: true,
        ..Default::default()
    })
    .await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    let request = SetFlagsRequest {
        message_id,
        flags: vec!["\\Flagged".to_owned()],
        idempotency_key: "flag-key".to_owned(),
    };
    let status = client
        .set_flags(request.clone())
        .await
        .expect_err("the fake IMAP refuses");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");

    // The same key again: a released claim, so the call runs rather than
    // replaying — and fails the same way, which is only observable because it
    // reached IMAP a second time.
    let status = client
        .set_flags(request)
        .await
        .expect_err("the fake IMAP still refuses");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert_eq!(
        server
            .imap_calls()
            .iter()
            .filter(|call| matches!(call, Call::SetFlags { .. }))
            .count(),
        2,
        "a failed mutation must leave its key retryable"
    );

    server.stop().await;
}

#[tokio::test]
async fn an_unusable_idempotency_key_is_invalid_argument() {
    let server = TestServer::start().await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    let status = client
        .delete(DeleteRequest {
            message_id,
            idempotency_key: "has\nnewline".to_owned(),
        })
        .await
        .expect_err("a key is echoed into logs; control characters are refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(
        server.imap_calls().is_empty(),
        "a rejected key must be rejected before the mutation runs"
    );

    server.stop().await;
}

#[tokio::test]
async fn mutations_without_a_key_are_unfenced_and_unchanged() {
    // The field is opt-in: a client that does not know about it must behave
    // exactly as it did before, including being able to repeat a call.
    let server = TestServer::start().await;
    let (_account_id, _inbox_id, _archive_id, message_id) = server.seed(&[], None);
    let mut client = server.client().await;

    for _ in 0..2 {
        client
            .set_flags(SetFlagsRequest {
                message_id,
                flags: vec!["\\Seen".to_owned()],
                idempotency_key: String::new(),
            })
            .await
            .unwrap();
    }
    assert_eq!(
        server
            .imap_calls()
            .iter()
            .filter(|call| matches!(call, Call::SetFlags { .. }))
            .count(),
        2,
        "an empty key must not fence anything"
    );

    server.stop().await;
}
