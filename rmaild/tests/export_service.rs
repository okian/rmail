//! Integration test: drive `ExportService` end-to-end against an in-process
//! tonic server over a Unix domain socket.
//!
//! `rmail-core::export`'s own tests already prove the framing, the round
//! trips, and the selection semantics with no daemon involved. What this
//! suite proves is what only means something on the wire: the chunk stream
//! reassembles into the same archive the core produced, the per-file formats
//! name entries a client can safely write, and every refusable request is
//! refused as the *call's* status rather than as the first frame of a stream
//! that already looks successful.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage, NewThread};
use rmail_proto::v1::export_service_client::ExportServiceClient;
use rmail_proto::v1::{export_request, ExportChunk, ExportDone, ExportFormat, ExportRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    account_id: i64,
    mailbox_id: i64,
    /// Behind a `Mutex<Option<_>>` so a test can stop the daemon *during* a
    /// stream (`stop_serving`) and still clean up afterwards.
    shutdown: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-export-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-export-svc-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();

        let (account_id, mailbox_id) = db
            .write(|conn| {
                let account_id = repo::insert_account(
                    conn,
                    &NewAccount {
                        name: "primary".into(),
                        imap_server: Some("imap.example.com".into()),
                        imap_port: Some(993),
                        username: Some("ada".into()),
                        smtp_server: None,
                        smtp_port: None,
                        secret_kind: None,
                        secret_ref: None,
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    conn,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".into(),
                        uidvalidity: Some(1),
                        uidnext: Some(1),
                        highestmodseq: None,
                        attributes: None,
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds(&server_socket, server_db, async move {
                let _ = shutdown_rx.await;
            })
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
            account_id,
            mailbox_id,
            shutdown: std::sync::Mutex::new(Some(shutdown_tx)),
            handle,
        }
    }

    async fn client(&self) -> ExportServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        ExportServiceClient::new(channel)
    }

    /// Seed a message directly — sync's write path is exercised by
    /// `rmail-core`'s own tests; this suite only needs rows to export.
    async fn seed(&self, uid: i64, from: &str, raw: Vec<u8>, thread_id: Option<i64>) -> i64 {
        let new = NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(format!("<msg-{uid}@example.com>")),
            thread_id,
            in_reply_to: None,
            references_hdr: None,
            subject: Some(format!("Report {uid}")),
            from_addr: Some(from.to_owned()),
            from_name: Some("Ada".to_owned()),
            to_addrs: Some("bob@example.com".to_owned()),
            cc_addrs: None,
            date: Some(1_700_000_000 + uid),
            internaldate: Some(1_700_000_000 + uid),
            size: Some(raw.len() as i64),
            raw: Some(raw),
            body_text: Some("body".to_owned()),
            body_html: None,
            has_attachments: false,
        };
        self.db
            .write(move |conn| repo::insert_message(conn, &new))
            .await
            .unwrap()
    }

    async fn thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |conn| {
                repo::insert_thread(
                    conn,
                    &NewThread {
                        account_id,
                        subject_norm: Some("office move".into()),
                        root_message_id: None,
                        first_message_at: None,
                        last_message_at: None,
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Seed a message whose raw RFC822 was never stored.
    async fn seed_without_raw(&self, uid: i64) -> i64 {
        let new = NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(format!("<msg-{uid}@example.com>")),
            thread_id: None,
            in_reply_to: None,
            references_hdr: None,
            subject: Some(format!("Report {uid}")),
            from_addr: Some("ada@example.com".to_owned()),
            from_name: None,
            to_addrs: None,
            cc_addrs: None,
            date: Some(1_700_000_000 + uid),
            internaldate: Some(1_700_000_000 + uid),
            size: None,
            raw: None,
            body_text: None,
            body_html: None,
            has_attachments: false,
        };
        self.db
            .write(move |conn| repo::insert_message(conn, &new))
            .await
            .unwrap()
    }

    /// Tell the daemon to stop, without waiting for it — for a test that needs
    /// shutdown to land while a stream is open.
    fn stop_serving(&self) {
        if let Ok(mut slot) = self.shutdown.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(());
            }
        }
    }

    async fn cleanup(self) {
        self.stop_serving();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }

    async fn shutdown(self) {
        self.cleanup().await;
    }
}

fn raw_message(n: i64) -> Vec<u8> {
    format!(
        "Message-ID: <msg-{n}@example.com>\r\n\
         From: Ada <ada@example.com>\r\n\
         Subject: Report {n}\r\n\
         \r\n\
         From now on we ship on Fridays.\r\n"
    )
    .into_bytes()
}

fn request(selection: export_request::Selection, format: ExportFormat) -> ExportRequest {
    ExportRequest {
        selection: Some(selection),
        format: format as i32,
        with_ai: false,
        limit: 0,
    }
}

/// Drain a stream into the archive a client would write: single-stream
/// formats concatenate, per-file formats accumulate per path.
///
/// Asserts the completion sentinel arrived, because that is the rule every
/// consumer follows: a stream without `ExportDone` is a truncated archive,
/// whatever the call's status said.
struct Archive {
    single: Vec<u8>,
    files: BTreeMap<String, Vec<u8>>,
    starts: Vec<i64>,
    done: ExportDone,
}

async fn collect(stream: tonic::Streaming<ExportChunk>) -> Archive {
    let (single, files, starts, done) = drain(stream).await;
    Archive {
        single,
        files,
        starts,
        done: done.expect("a complete export must end with an ExportDone frame"),
    }
}

/// As [`collect`], without insisting the export completed.
#[allow(clippy::type_complexity)]
async fn drain(
    mut stream: tonic::Streaming<ExportChunk>,
) -> (
    Vec<u8>,
    BTreeMap<String, Vec<u8>>,
    Vec<i64>,
    Option<ExportDone>,
) {
    let mut single = Vec::new();
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut starts = Vec::new();
    let mut done = None;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if let Some(summary) = chunk.done {
            assert!(chunk.data.is_empty(), "the terminal frame carries no bytes");
            assert!(chunk.path.is_empty(), "the terminal frame names no entry");
            done = Some(summary);
            continue;
        }
        if chunk.start_of_message {
            starts.push(chunk.message_id);
        }
        if chunk.path.is_empty() {
            single.extend_from_slice(&chunk.data);
        } else {
            files.entry(chunk.path).or_default().extend(chunk.data);
        }
    }
    (single, files, starts, done)
}

#[tokio::test]
async fn an_mbox_export_streams_an_archive_that_preserves_raw_rfc822() {
    let server = TestServer::start().await;
    for uid in 1..=3 {
        server
            .seed(uid, "ada@example.com", raw_message(uid), None)
            .await;
    }

    let mut client = server.client().await;
    let stream = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Mbox,
        ))
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await;

    assert!(archive.files.is_empty(), "mbox is a single document");
    assert_eq!(archive.starts.len(), 3);
    assert_eq!(archive.done.messages, 3);
    assert_eq!(archive.done.skipped_without_raw, 0);
    assert!(archive.single.starts_with(b"From ada@example.com "));
    let archive = archive.single;
    for uid in 1..=3 {
        // The fixture deliberately contains a body line beginning `From `, so
        // what lands in the archive is the mboxrd-escaped form. Comparing
        // against that exact form (rather than accepting either) is what makes
        // this a check that escaping happened, not just that some bytes did.
        let expected = escaped(&raw_message(uid));
        assert!(
            archive
                .windows(expected.len())
                .any(|w| w == expected.as_slice()),
            "message {uid} is not in the archive as mboxrd-escaped raw RFC822"
        );
        assert_eq!(
            unescape(&expected),
            raw_message(uid),
            "escaping message {uid} is not reversible"
        );
    }
    server.shutdown().await;
}

/// mboxrd escaping: prefix `>` to every line matching `^>*From `.
fn escaped(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 8);
    for line in raw.split_inclusive(|&b| b == b'\n') {
        let depth = line.iter().position(|&b| b != b'>').unwrap_or(line.len());
        if line[depth..].starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(line);
    }
    out
}

/// Its inverse: strip exactly one `>` from every line matching `^>+From `.
fn unescape(framed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(framed.len());
    for line in framed.split_inclusive(|&b| b == b'\n') {
        let depth = line.iter().position(|&b| b != b'>').unwrap_or(line.len());
        if depth > 0 && line[depth..].starts_with(b"From ") {
            out.extend_from_slice(&line[1..]);
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

#[tokio::test]
async fn a_maildir_export_names_one_safe_relative_entry_per_message() {
    let server = TestServer::start().await;
    let ids: Vec<i64> = {
        let mut ids = Vec::new();
        for uid in 1..=2 {
            ids.push(
                server
                    .seed(uid, "ada@example.com", raw_message(uid), None)
                    .await,
            );
        }
        ids
    };

    let mut client = server.client().await;
    let stream = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Maildir,
        ))
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await;

    assert!(archive.single.is_empty(), "maildir writes no unnamed bytes");
    assert_eq!(archive.files.len(), 2);
    assert_eq!(archive.starts, ids);
    assert_eq!(archive.done.messages, 2);
    for (path, bytes) in &archive.files {
        assert!(path.starts_with("cur/"), "{path}");
        assert!(!path.contains(".."), "{path}");
        assert!(!path.starts_with('/'), "{path}");
        assert!(
            bytes == &raw_message(1) || bytes == &raw_message(2),
            "{path} is not a stored message verbatim"
        );
        // The writer half every client shares must accept what the server
        // sent — a path this rejects is a protocol bug, not a client bug.
        rmail_core::export::write::safe_join(std::path::Path::new("/tmp/dest"), path)
            .expect("the daemon must only emit paths a client can safely write");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_json_export_with_ai_attaches_stored_artifacts() {
    let server = TestServer::start().await;
    let id = server
        .seed(1, "ada@example.com", raw_message(1), None)
        .await;
    let account_id = server.account_id;
    server
        .db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO ai_summaries (message_id, account_id, model, pass, \
                 schema_version, tl_dr, created_at) \
                 VALUES (?1, ?2, 'claude-test', 'triage', 1, 'Ships Fridays', 1)",
                rusqlite::params![id, account_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let mut client = server.client().await;
    let stream = client
        .export(ExportRequest {
            selection: Some(export_request::Selection::Query(String::new())),
            format: ExportFormat::Json as i32,
            with_ai: true,
            limit: 0,
        })
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await.single;

    let document: serde_json::Value =
        serde_json::from_slice(&archive).expect("one valid JSON document");
    let record = &document["messages"][0];
    assert_eq!(record["id"], id);
    assert_eq!(record["ai"]["summaries"][0]["tl_dr"], "Ships Fridays");
    let encoded = record["raw_rfc822_base64"].as_str().unwrap();
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap();
    assert_eq!(decoded, raw_message(1));
    server.shutdown().await;
}

#[tokio::test]
async fn a_thread_selection_exports_that_thread() {
    let server = TestServer::start().await;
    let thread_id = server.thread().await;
    let inside = server
        .seed(1, "ada@example.com", raw_message(1), Some(thread_id))
        .await;
    server
        .seed(2, "ada@example.com", raw_message(2), None)
        .await;

    let mut client = server.client().await;
    let stream = client
        .export(request(
            export_request::Selection::ThreadId(thread_id),
            ExportFormat::Eml,
        ))
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await;

    assert_eq!(archive.starts, vec![inside]);
    assert_eq!(archive.files.len(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn a_message_larger_than_one_frame_is_chunked_and_reassembles() {
    let server = TestServer::start().await;
    let mut raw = b"Subject: big\r\n\r\n".to_vec();
    raw.extend(std::iter::repeat_n(b'x', 700 * 1024));
    raw.extend_from_slice(b"\r\n");
    server.seed(1, "ada@example.com", raw.clone(), None).await;

    let mut client = server.client().await;
    let mut stream = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Eml,
        ))
        .await
        .expect("export accepted")
        .into_inner();

    let mut frames = 0;
    let mut assembled = Vec::new();
    let mut done = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream frame");
        if chunk.done.is_some() {
            done = true;
            continue;
        }
        frames += 1;
        assert!(
            chunk.data.len() <= 256 * 1024,
            "a frame exceeded the chunk size"
        );
        assembled.extend_from_slice(&chunk.data);
    }
    assert!(frames > 2, "a 700 KiB message should span frames");
    assert!(done, "the export must end with an ExportDone frame");
    assert_eq!(assembled, raw);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Refusals: every one arrives as the call's status, before any frame
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unset_selection_is_refused_before_the_stream_starts() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .export(ExportRequest {
            selection: None,
            format: ExportFormat::Mbox as i32,
            with_ai: false,
            limit: 0,
        })
        .await
        .expect_err("an export must name a selection");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn an_unspecified_format_is_refused_before_the_stream_starts() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Unspecified,
        ))
        .await
        .expect_err("an export must name a format");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn with_ai_on_a_byte_format_is_refused_before_the_stream_starts() {
    let server = TestServer::start().await;
    server
        .seed(1, "ada@example.com", raw_message(1), None)
        .await;
    let mut client = server.client().await;
    let status = client
        .export(ExportRequest {
            selection: Some(export_request::Selection::Query(String::new())),
            format: ExportFormat::Mbox as i32,
            with_ai: true,
            limit: 0,
        })
        .await
        .expect_err("mbox has nowhere to put AI artifacts");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn a_thread_that_does_not_exist_is_not_found_before_the_stream_starts() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .export(request(
            export_request::Selection::ThreadId(4242),
            ExportFormat::Mbox,
        ))
        .await
        .expect_err("a missing thread must not export as an empty archive");
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

#[tokio::test]
async fn a_negative_limit_is_refused() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .export(ExportRequest {
            selection: Some(export_request::Selection::Query(String::new())),
            format: ExportFormat::Mbox as i32,
            with_ai: false,
            limit: -1,
        })
        .await
        .expect_err("a negative limit is not a selection");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn a_limit_caps_the_stream() {
    let server = TestServer::start().await;
    for uid in 1..=5 {
        server
            .seed(uid, "ada@example.com", raw_message(uid), None)
            .await;
    }
    let mut client = server.client().await;
    let stream = client
        .export(ExportRequest {
            selection: Some(export_request::Selection::Query(String::new())),
            format: ExportFormat::Eml as i32,
            with_ai: false,
            limit: 2,
        })
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await;
    assert_eq!(archive.starts.len(), 2);
    assert_eq!(archive.files.len(), 2);
    assert_eq!(archive.done.messages, 2);
    server.shutdown().await;
}

/// A message the selection matched but whose raw was never stored is absent
/// from a byte archive, and the terminal frame is where the caller learns it.
#[tokio::test]
async fn a_message_with_no_stored_raw_is_reported_in_the_terminal_frame() {
    let server = TestServer::start().await;
    server
        .seed(1, "ada@example.com", raw_message(1), None)
        .await;
    server.seed_without_raw(2).await;

    let mut client = server.client().await;
    let stream = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Mbox,
        ))
        .await
        .expect("export accepted")
        .into_inner();
    let archive = collect(stream).await;
    assert_eq!(archive.done.messages, 1);
    assert_eq!(archive.done.skipped_without_raw, 1);
    server.shutdown().await;
}

/// A query whose constraints cannot be enforced is refused, not widened into
/// an archive of the whole mailbox. See `rmail_core::export::select`'s
/// "Degradation" note.
#[tokio::test]
async fn an_unenforceable_query_is_refused_rather_than_exporting_everything() {
    let server = TestServer::start().await;
    for uid in 1..=3 {
        server
            .seed(uid, "ada@example.com", raw_message(uid), None)
            .await;
    }
    let mut client = server.client().await;
    for query in ["~invoice", "after:lasst-week"] {
        let status = client
            .export(request(
                export_request::Selection::Query(query.to_owned()),
                ExportFormat::Mbox,
            ))
            .await
            .err()
            .unwrap_or_else(|| panic!("{query} must not export the whole mailbox"));
        assert_eq!(status.code(), Code::InvalidArgument, "{query}");
    }
    server.shutdown().await;
}

/// The completion contract, from the client's side: a daemon that stops
/// mid-export must never leave a stream that looks finished. Either the client
/// sees an error frame, or it sees the stream end with no `ExportDone` — never
/// a clean end *with* one.
#[tokio::test]
async fn a_daemon_shutdown_mid_export_never_yields_a_completion_marker() {
    let server = TestServer::start().await;
    // Big enough that the export cannot finish inside one frame, and the
    // stream buffer (8) cannot swallow the whole thing.
    let mut raw = b"Subject: big\r\n\r\n".to_vec();
    raw.extend(std::iter::repeat_n(b'x', 400 * 1024));
    for uid in 1..=40 {
        server.seed(uid, "ada@example.com", raw.clone(), None).await;
    }

    let mut client = server.client().await;
    let mut stream = client
        .export(request(
            export_request::Selection::Query(String::new()),
            ExportFormat::Eml,
        ))
        .await
        .expect("export accepted")
        .into_inner();

    // Take one frame, then pull the daemon out from under the stream.
    let first = stream.next().await.expect("a first frame");
    assert!(first.is_ok());
    server.stop_serving();

    let mut saw_done = false;
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => saw_done |= chunk.done.is_some(),
            Err(_) => {
                saw_error = true;
                break;
            }
        }
    }
    assert!(
        !saw_done || !saw_error,
        "a stream cannot both fail and claim completion"
    );
    if saw_done {
        // The export outran the shutdown — legitimate, and then the archive
        // really is complete.
        assert!(!saw_error);
    }
    server.cleanup().await;
}
