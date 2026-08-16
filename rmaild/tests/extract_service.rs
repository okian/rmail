//! Integration test: drive `ExtractService`, `LinkService` and
//! `AttachmentService.ExtractTables` end-to-end against an in-process tonic
//! server over a Unix domain socket.
//!
//! What this covers that the unit tests cannot:
//!
//! - The engine is actually *wired*. Task 57 shipped a mechanism no operator
//!   could enable; every RPC here is the check that this one is reachable from
//!   outside `rmail-core`, with a real MIME message rather than a hand-built
//!   part list.
//! - The MIME decode agrees with the rest of the daemon: `ExtractTables`'
//!   `part_id` is the same positional identity `rmail_core::attach` assigns, so
//!   a part id from an attachment search names the same bytes here.
//! - Idempotency survives the boundary. Two `ExtractEvents` calls on one
//!   message return the events twice and deliver them once, which is the
//!   entire point of `extraction_deliveries` and is not observable from a unit
//!   test of the claim table alone.
//! - The enums cross the wire as themselves: an inferred table, a `MODEL`
//!   source, a `TRACKING` kind, a `deceptive` flag.
//! - The error paths a client can actually reach are the right codes.
//!
//! No provider is involved anywhere: every route exercised here is one of the
//! deterministic ones, which is exactly the claim the module docs make about
//! what works with AI switched off.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use rmail_proto::v1::attachment_service_client::AttachmentServiceClient;
use rmail_proto::v1::extract_service_client::ExtractServiceClient;
use rmail_proto::v1::link_service_client::LinkServiceClient;
use rmail_proto::v1::{
    CellType, ExtractEventsRequest, ExtractLinksRequest, ExtractTablesRequest, ExtractTasksRequest,
    ExtractionSink, ExtractionSource, LinkKind, TableOrigin,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: AtomicI64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-extract-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-extract-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = rmail_core::repo::insert_account(
                    c,
                    &rmail_core::repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = rmail_core::repo::insert_mailbox(
                    c,
                    &rmail_core::repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
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
            next_uid: AtomicI64::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn channel(&self) -> Channel {
        rmail_core::connect_uds(&self.socket).await.unwrap()
    }

    /// Store a message with `raw` as its RFC822 bytes — the extractors read
    /// the stored raw, not the parsed columns, so this is the only field that
    /// matters to them.
    async fn message(&self, subject: &str, raw: &str) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let new = rmail_core::repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            from_addr: Some("ada@example.com".to_owned()),
            raw: Some(raw.as_bytes().to_vec()),
            ..Default::default()
        };
        self.db
            .write(move |c| rmail_core::repo::insert_message(c, &new))
            .await
            .unwrap()
    }

    /// A message with no stored body at all, for the precondition path.
    async fn headers_only(&self) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let new = rmail_core::repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some("Envelope only".to_owned()),
            ..Default::default()
        };
        self.db
            .write(move |c| rmail_core::repo::insert_message(c, &new))
            .await
            .unwrap()
    }

    async fn shutdown(self) {
        self.shutdown.send(()).unwrap();
        self.handle.await.unwrap().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A meeting invite: a multipart message with a real `text/calendar` part.
fn invite_raw() -> String {
    let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Example//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:invite-1@example.com\r\n\
SUMMARY:Quarterly review\r\n\
LOCATION:Room 4\r\n\
DTSTART:20240115T140000Z\r\n\
DTEND:20240115T150000Z\r\n\
ORGANIZER:mailto:ada@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VTODO\r\n\
UID:todo-1@example.com\r\n\
SUMMARY:Send the deck beforehand\r\n\
DUE:20240114T170000Z\r\n\
PRIORITY:2\r\n\
END:VTODO\r\n\
END:VCALENDAR\r\n";
    format!(
        "From: ada@example.com\r\n\
To: grace@example.com\r\n\
Subject: Quarterly review\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain\r\n\
\r\n\
See the attached invite.\r\n\
--b1\r\n\
Content-Type: text/calendar; method=REQUEST\r\n\
\r\n\
{ics}\r\n\
--b1--\r\n"
    )
}

/// A newsletter whose HTML carries a spoofed link, a tracker, a meeting link,
/// a declared unsubscribe and a beacon.
fn newsletter_raw() -> String {
    "From: news@example.com\r\n\
To: grace@example.com\r\n\
Subject: This week\r\n\
List-Unsubscribe: <https://example.com/p/9f2b>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html\r\n\
\r\n\
<html><body>\
<a href=\"https://evil.example.net/login\">https://bank.example.com</a>\
<a href=\"https://zoom.us/j/98765\">Join the call</a>\
<a href=\"https://track.list-manage.com/click?u=1\">Read more</a>\
<a href=\"https://example.com/p/9f2b\">Manage preferences</a>\
<img src=\"https://track.example.com/o.gif\" width=\"1\" height=\"1\">\
</body></html>\r\n"
        .to_owned()
}

/// A message with a CSV attachment, so `ExtractTables` has real bytes and a
/// real positional part id to resolve.
fn csv_attachment_raw() -> String {
    "From: ada@example.com\r\n\
To: grace@example.com\r\n\
Subject: October numbers\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain\r\n\
\r\n\
Numbers attached.\r\n\
--b1\r\n\
Content-Type: text/csv; name=\"october.csv\"\r\n\
Content-Disposition: attachment; filename=\"october.csv\"\r\n\
\r\n\
Item,Qty,Price\r\n\
Widget,3,19.99\r\n\
Gadget,1,5\r\n\
--b1--\r\n"
        .to_owned()
}

// ---------------------------------------------------------------------------
// ExtractEvents / ExtractTasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_invite_crosses_the_wire_as_a_normalized_event_and_is_delivered_once() {
    let server = TestServer::start().await;
    let mut client = ExtractServiceClient::new(server.channel().await);
    let message_id = server.message("Quarterly review", &invite_raw()).await;

    let first = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect("extract_events")
        .into_inner();

    assert_eq!(first.events.len(), 1);
    let event = &first.events[0];
    assert_eq!(event.uid, "invite-1@example.com");
    assert_eq!(event.summary, "Quarterly review");
    assert_eq!(event.location, "Room 4");
    assert_eq!(event.starts_at, 1_705_327_200);
    assert_eq!(event.ends_at, 1_705_330_800);
    assert_eq!(event.source, ExtractionSource::Ics as i32);
    assert_eq!(
        first.method, "REQUEST",
        "a client must be able to tell an invitation from a cancellation"
    );
    assert!(
        first.ics.contains("SUMMARY:Quarterly review"),
        "the rendered file comes back whatever the sink"
    );
    assert_eq!(first.delivered, 1);
    assert_eq!(first.already_delivered, 0);

    // The same call again: same answer, no second delivery.
    let second = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect("extract_events")
        .into_inner();
    assert_eq!(second.events.len(), 1, "the extraction is not consumed");
    assert_eq!(second.delivered, 0, "but the delivery is");
    assert_eq!(second.already_delivered, 1);
    assert!(
        !second.ics.is_empty(),
        "asking for the file a second time still gets the file"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn tasks_are_delivered_under_their_own_kind_so_events_do_not_suppress_them() {
    let server = TestServer::start().await;
    let channel = server.channel().await;
    let mut client = ExtractServiceClient::new(channel);
    let message_id = server.message("Quarterly review", &invite_raw()).await;

    let events = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect("extract_events")
        .into_inner();
    assert_eq!(events.delivered, 1);

    let tasks = client
        .extract_tasks(ExtractTasksRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect("extract_tasks")
        .into_inner();
    assert_eq!(tasks.tasks.len(), 1);
    assert_eq!(tasks.tasks[0].summary, "Send the deck beforehand");
    assert_eq!(tasks.tasks[0].priority, 2);
    assert_eq!(
        tasks.delivered, 1,
        "the event delivery must not have claimed the task's uid"
    );
    assert!(tasks.ics.contains("BEGIN:VTODO"));

    server.shutdown().await;
}

#[tokio::test]
async fn an_unconfigured_sink_is_refused_rather_than_silently_returning_a_file() {
    // The daemon under test has no `extract.command`, and a user must not
    // believe their reminder was created when nothing ran.
    let server = TestServer::start().await;
    let mut client = ExtractServiceClient::new(server.channel().await);
    let message_id = server.message("Quarterly review", &invite_raw()).await;

    let status = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Command as i32,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("extract.command"),
        "the message names what to configure: {}",
        status.message()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_model_route_that_cannot_run_fails_loudly_rather_than_degrading() {
    // The invariant, not the environment: `use_model` asks for events the
    // `.ics` does not state, and a caller must be able to tell "the model
    // found nothing" from "there was no model". Silently returning the
    // deterministic answer would make those two indistinguishable, which for a
    // calendar means a meeting the user was told about and never got.
    //
    // This daemon has no credentials, so the exact failure depends on how far
    // the call gets: `INVALID_ARGUMENT` when no provider was built at all,
    // `UNAUTHENTICATED` when one was built and the key could not be resolved,
    // `FAILED_PRECONDITION` when `ai.policy` or a spend cap refuses. All three
    // are loud; success is the one answer that would be wrong.
    let server = TestServer::start().await;
    let mut client = ExtractServiceClient::new(server.channel().await);
    let message_id = server.message("Quarterly review", &invite_raw()).await;

    let status = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: true,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect_err("a model route with no model must not answer");
    assert!(
        matches!(
            status.code(),
            Code::InvalidArgument
                | Code::Unauthenticated
                | Code::FailedPrecondition
                | Code::ResourceExhausted
        ),
        "unexpected code {:?}: {}",
        status.code(),
        status.message()
    );

    // And the deterministic route on the same message still answers, which is
    // what makes the failure above a refusal rather than an outage.
    let ok = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect("the .ics route is unaffected")
        .into_inner();
    assert_eq!(ok.events.len(), 1);

    server.shutdown().await;
}

#[tokio::test]
async fn extracting_from_a_message_with_no_body_is_a_precondition_not_a_not_found() {
    let server = TestServer::start().await;
    let mut client = ExtractServiceClient::new(server.channel().await);
    let message_id = server.headers_only().await;

    let status = client
        .extract_events(ExtractEventsRequest {
            message_id,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect_err("declined");
    assert_eq!(
        status.code(),
        Code::FailedPrecondition,
        "the message exists; the pipeline is simply not far enough along"
    );

    let status = client
        .extract_events(ExtractEventsRequest {
            message_id: 999_999,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::NotFound);

    let status = client
        .extract_events(ExtractEventsRequest {
            message_id: 0,
            use_model: false,
            sink: ExtractionSink::Ics as i32,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// ExtractLinks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_link_picker_floats_the_meeting_and_flags_the_spoof() {
    let server = TestServer::start().await;
    let mut client = LinkServiceClient::new(server.channel().await);
    let message_id = server.message("This week", &newsletter_raw()).await;

    let response = client
        .extract_links(ExtractLinksRequest {
            message_id,
            use_model: false,
        })
        .await
        .expect("extract_links")
        .into_inner();

    assert_eq!(
        response.links.first().map(|link| link.kind),
        Some(LinkKind::Meeting as i32),
        "the picker floats what the reader opened the mail for: {:?}",
        response
            .links
            .iter()
            .map(|link| (link.url.clone(), link.kind, link.score))
            .collect::<Vec<_>>()
    );

    let spoof = response
        .links
        .iter()
        .find(|link| link.host == "evil.example.net")
        .expect("the spoofed link is still listed");
    assert!(spoof.deceptive, "the mismatch is reported, not hidden");
    assert_eq!(spoof.display_host, "bank.example.com");
    assert_eq!(
        spoof.display_text, "https://bank.example.com",
        "the claim the message made is preserved for the reader to see"
    );

    let unsubscribe = response
        .links
        .iter()
        .find(|link| link.url.contains("/p/9f2b"))
        .expect("the header-declared unsubscribe");
    assert_eq!(unsubscribe.kind, LinkKind::Unsubscribe as i32);

    let tracker = response
        .links
        .iter()
        .find(|link| link.host.ends_with("list-manage.com"))
        .expect("the redirector");
    assert_eq!(tracker.kind, LinkKind::Tracking as i32);
    assert!(
        tracker.reason.contains("not resolved"),
        "and the daemon says it did not follow it: {:?}",
        tracker.reason
    );

    assert_eq!(
        response.tracking_pixels, 1,
        "the beacon is counted, never offered as something to click"
    );
    assert!(
        response
            .links
            .iter()
            .all(|link| !link.url.contains("o.gif")),
        "offering the beacon would fire it"
    );
    assert!(
        response
            .links
            .iter()
            .all(|link| link.scheme == "http" || link.scheme == "https"),
        "only http(s) reaches a client"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_message_with_no_body_cannot_be_scanned_for_links() {
    let server = TestServer::start().await;
    let mut client = LinkServiceClient::new(server.channel().await);
    let message_id = server.headers_only().await;

    let status = client
        .extract_links(ExtractLinksRequest {
            message_id,
            use_model: false,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::FailedPrecondition);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// ExtractTables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_csv_attachment_becomes_typed_rows_over_the_wire() {
    let server = TestServer::start().await;
    let mut client = AttachmentServiceClient::new(server.channel().await);
    let message_id = server
        .message("October numbers", &csv_attachment_raw())
        .await;

    let response = client
        .extract_tables(ExtractTablesRequest {
            message_id,
            // The positional identity `rmail_core::attach` assigns: the first
            // attachment of the message.
            part_id: "0".to_owned(),
            allow_model: false,
        })
        .await
        .expect("extract_tables")
        .into_inner();

    let table = response.tables.first().expect("one table");
    assert_eq!(table.origin, TableOrigin::Csv as i32);
    assert!(
        !table.inferred,
        "a parsed table must not read as a transcription"
    );
    assert_eq!(
        table
            .columns
            .iter()
            .map(|column| column.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Item", "Qty", "Price"]
    );
    assert_eq!(table.columns[2].r#type, CellType::Number as i32);
    assert_eq!(table.rows.len(), 2);
    let price = &table.rows[0].cells[2];
    assert_eq!(price.r#type, CellType::Number as i32);
    assert!((price.number - 19.99).abs() < 1e-9);
    assert_eq!(
        price
            .source
            .as_ref()
            .map(|source| source.reference.as_str()),
        Some("C2"),
        "provenance survives the boundary"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_part_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;
    let mut client = AttachmentServiceClient::new(server.channel().await);
    let message_id = server
        .message("October numbers", &csv_attachment_raw())
        .await;

    let status = client
        .extract_tables(ExtractTablesRequest {
            message_id,
            part_id: "17".to_owned(),
            allow_model: false,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::NotFound);

    server.shutdown().await;
}

#[tokio::test]
async fn a_plain_text_attachment_has_no_table_route_and_says_which() {
    let server = TestServer::start().await;
    let mut client = AttachmentServiceClient::new(server.channel().await);
    // A `.txt` attachment: a recognized format with no tabular structure at
    // all. Distinguished from a PDF, which has one this build can only infer.
    let raw = "From: ada@example.com\r\n\
Subject: Notes\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain\r\n\
\r\n\
body\r\n\
--b1\r\n\
Content-Type: text/plain; name=\"notes.txt\"\r\n\
Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
\r\n\
just some prose with no delimiters at all\r\n\
--b1--\r\n";
    let message_id = server.message("Notes", raw).await;

    let status = client
        .extract_tables(ExtractTablesRequest {
            message_id,
            part_id: "0".to_owned(),
            allow_model: false,
        })
        .await
        .expect_err("declined");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}
