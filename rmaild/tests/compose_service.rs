//! Integration test: drive `ComposeService` end-to-end against an in-process
//! tonic server over a Unix domain socket, backed by a real
//! `rmail_core::compose::DraftStore` over a real (temp-file) database — the
//! same "build the handler directly, no fake transport" discipline
//! `note_service.rs`'s harness uses, and for the same reason: every
//! dependency of this service is already local, so there is nothing worth
//! faking.
//!
//! Covers task 60's acceptance bullets at the gRPC boundary: draft CRUD, the
//! `NOT_FOUND`/`INVALID_ARGUMENT` paths, local persistence across a reopen,
//! and `RenderDraft` producing a message that parses back into the draft that
//! went in — including a reply's `In-Reply-To`/`References` and the rule that
//! `Bcc` reaches the envelope and never the message.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use rmail_core::compose::DraftStore;
use rmail_core::message::parse_message;
use rmail_core::repo;
use rmail_core::Database;
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::{
    CreateDraftRequest, DeleteDraftRequest, Draft, DraftAddress, DraftAddressList,
    DraftAttachmentList, GetDraftRequest, ListDraftsRequest, NewDraftAttachment,
    RenderDraftRequest, UpdateDraftRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: AtomicI64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-compose-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-compose-svc-{pid}-{n}.db"));
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

        let api = rmaild::ComposeApi::new(DraftStore::new(db.clone()));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(
                    rmail_proto::v1::compose_service_server::ComposeServiceServer::new(api),
                )
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
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
            next_uid: AtomicI64::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> ComposeServiceClient<Channel> {
        ComposeServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// Insert a parent message with the given threading headers.
    async fn message(&self, message_id: &str, references: Option<&str>) -> i64 {
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid: self.next_uid.fetch_add(1, Ordering::Relaxed),
            uidvalidity: 1,
            message_id: Some(message_id.to_owned()),
            references_hdr: references.map(str::to_owned),
            subject: Some("Parent".to_owned()),
            ..Default::default()
        };
        self.db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .unwrap()
    }

    fn create_request(&self) -> CreateDraftRequest {
        CreateDraftRequest {
            account_id: self.account_id,
            from: Some(address("alice@example.com", "Alice")),
            to: vec![address("bob@example.net", "")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Lunch".to_owned(),
            body_text: "Shall we say noon?".to_owned(),
            body_html: None,
            attachments: Vec::new(),
            in_reply_to_message_id: None,
        }
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

fn address(addr: &str, display_name: &str) -> DraftAddress {
    DraftAddress {
        address: addr.to_owned(),
        display_name: display_name.to_owned(),
    }
}

fn addresses(draft: &[DraftAddress]) -> Vec<&str> {
    draft.iter().map(|a| a.address.as_str()).collect()
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_get_update_and_delete_round_trip() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created: Draft = client
        .create_draft(CreateDraftRequest {
            cc: vec![address("carol@example.org", "Carol")],
            body_html: Some("<p>noon?</p>".to_owned()),
            attachments: vec![NewDraftAttachment {
                filename: "notes.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                content: b"attached".to_vec(),
            }],
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    assert!(created.id > 0);
    assert_eq!(created.from.as_ref().unwrap().display_name, "Alice");
    assert_eq!(addresses(&created.to), vec!["bob@example.net"]);
    assert_eq!(addresses(&created.cc), vec!["carol@example.org"]);
    assert_eq!(created.body_html.as_deref(), Some("<p>noon?</p>"));
    assert_eq!(created.attachments.len(), 1);
    assert_eq!(created.attachments[0].content, b"attached");
    assert_eq!(created.attachments[0].size, 8);

    let fetched = client
        .get_draft(GetDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fetched, created);

    let updated = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            subject: Some("Dinner".to_owned()),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(updated.subject, "Dinner");
    assert_eq!(
        updated.body_text, created.body_text,
        "unset fields untouched"
    );
    assert_eq!(addresses(&updated.cc), vec!["carol@example.org"]);
    assert_eq!(updated.attachments.len(), 1);

    client
        .delete_draft(DeleteDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap();
    let status = client
        .get_draft(GetDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

#[tokio::test]
async fn drafts_persist_locally_across_a_reopen() {
    // The acceptance bullet's "drafts persist locally", proven at the gRPC
    // boundary: the draft survives the server (and its database handle) going
    // away entirely.
    let server = TestServer::start().await;
    let id = server
        .client()
        .await
        .create_draft(server.create_request())
        .await
        .unwrap()
        .into_inner()
        .id;
    let db_path = server.db_path.clone();
    let socket = server.socket.clone();
    let _ = server.shutdown.send(());
    let _ = server.handle.await;
    let _ = std::fs::remove_file(&socket);

    let reopened = Database::open(&db_path).unwrap();
    let draft = DraftStore::new(reopened).get(id).await.unwrap();
    assert_eq!(draft.subject, "Lunch");

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
}

#[tokio::test]
async fn list_omits_attachment_bytes_but_keeps_the_size() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .create_draft(CreateDraftRequest {
            attachments: vec![NewDraftAttachment {
                filename: "big.bin".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                content: vec![9u8; 2048],
            }],
            ..server.create_request()
        })
        .await
        .unwrap();

    let listed = client
        .list_drafts(ListDraftsRequest {
            account_id: server.account_id,
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .drafts;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].attachments[0].size, 2048);
    assert!(
        listed[0].attachments[0].content.is_empty(),
        "a list must not stream every attachment of every draft"
    );

    let full = client
        .get_draft(GetDraftRequest {
            draft_id: listed[0].id,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(full.attachments[0].content.len(), 2048);

    server.stop().await;
}

#[tokio::test]
async fn an_unset_recipient_list_is_left_alone_and_an_empty_one_clears_it() {
    // The distinction proto3 cannot express with a bare `repeated`, which is
    // why the request wraps each list — see `compose_service`'s module docs.
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created = client
        .create_draft(CreateDraftRequest {
            cc: vec![address("carol@example.org", "")],
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    // Unset: untouched.
    let untouched = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            subject: Some("still here".to_owned()),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(addresses(&untouched.cc), vec!["carol@example.org"]);

    // Set-but-empty: cleared.
    let cleared = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            cc: Some(DraftAddressList {
                addresses: Vec::new(),
            }),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(cleared.cc.is_empty());
    assert_eq!(
        addresses(&cleared.to),
        vec!["bob@example.net"],
        "To survives"
    );

    // The same for attachments.
    let with_attachment = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            attachments: Some(DraftAttachmentList {
                attachments: vec![NewDraftAttachment {
                    filename: "a.txt".to_owned(),
                    content_type: "text/plain".to_owned(),
                    content: b"x".to_vec(),
                }],
            }),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(with_attachment.attachments.len(), 1);
    let emptied = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            attachments: Some(DraftAttachmentList {
                attachments: Vec::new(),
            }),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(emptied.attachments.is_empty());

    server.stop().await;
}

#[tokio::test]
async fn clearing_the_html_alternative_uses_the_empty_string() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created = client
        .create_draft(CreateDraftRequest {
            body_html: Some("<p>original</p>".to_owned()),
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(created.body_html.is_some());

    let cleared = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            body_html: Some(String::new()),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cleared.body_html, None);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn editing_or_deleting_a_missing_draft_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    assert_eq!(
        client
            .get_draft(GetDraftRequest { draft_id: 4242 })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .update_draft(UpdateDraftRequest {
                draft_id: 4242,
                subject: Some("x".to_owned()),
                ..UpdateDraftRequest::default()
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .delete_draft(DeleteDraftRequest { draft_id: 4242 })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .render_draft(RenderDraftRequest { draft_id: 4242 })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );

    server.stop().await;
}

#[tokio::test]
async fn a_draft_with_no_recipients_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_draft(CreateDraftRequest {
            to: Vec::new(),
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // Nor can an edit get there.
    let created = client
        .create_draft(server.create_request())
        .await
        .unwrap()
        .into_inner();
    let status = client
        .update_draft(UpdateDraftRequest {
            draft_id: created.id,
            to: Some(DraftAddressList {
                addresses: Vec::new(),
            }),
            ..UpdateDraftRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn an_unparseable_address_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    for bad in [
        "not-an-address",
        "two@at@signs.com",
        "",
        // Header injection through the address field.
        "bob@example.net>\r\nBcc: victim@example.org",
    ] {
        let status = client
            .create_draft(CreateDraftRequest {
                to: vec![address(bad, "")],
                ..server.create_request()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{bad:?}");
    }

    // And in `from`, which is not a recipient but is just as much a header.
    let status = client
        .create_draft(CreateDraftRequest {
            from: Some(address("nonsense", "")),
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // A missing `from` is likewise a bad request, not a default.
    let status = client
        .create_draft(CreateDraftRequest {
            from: None,
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn a_subject_with_a_control_character_is_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .create_draft(CreateDraftRequest {
            subject: "Hi\r\nBcc: victim@example.org".to_owned(),
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn oversized_attachments_are_resource_exhausted_over_the_wire() {
    // The point of this test is not the domain rule (`rmail-core`'s own
    // suite covers that) — it is that the rule is *reachable* through gRPC.
    // `MAX_ATTACHMENT_BYTES` is deliberately set below tonic's default 4 MiB
    // codec limit precisely so an over-limit draft gets this clean
    // `RESOURCE_EXHAUSTED` instead of an opaque codec `OUT_OF_RANGE` that
    // explains nothing. Raise the constant past the transport limit and this
    // test is what fails.
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .create_draft(CreateDraftRequest {
            attachments: vec![NewDraftAttachment {
                filename: "huge.bin".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                content: vec![0u8; rmail_core::compose::MAX_ATTACHMENT_BYTES + 1],
            }],
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_draft_at_the_attachment_limit_still_renders_within_the_frame() {
    // The other half: a draft right at the ceiling must survive both the
    // request and the base64-inflated `RenderDraft` response on a
    // default-configured client.
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let created = client
        .create_draft(CreateDraftRequest {
            attachments: vec![NewDraftAttachment {
                filename: "big.bin".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                content: vec![0x5au8; rmail_core::compose::MAX_ATTACHMENT_BYTES],
            }],
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    let rendered = client
        .render_draft(RenderDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap()
        .into_inner();
    let parsed = parse_message(&rendered.mime);
    assert_eq!(
        parsed.attachments[0].size,
        Some(i64::try_from(rmail_core::compose::MAX_ATTACHMENT_BYTES).unwrap())
    );
    server.stop().await;
}

#[tokio::test]
async fn replying_to_a_message_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .create_draft(CreateDraftRequest {
            in_reply_to_message_id: Some(9999),
            ..server.create_request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    server.stop().await;
}

#[tokio::test]
async fn a_negative_page_size_is_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .list_drafts(ListDraftsRequest {
            account_id: server.account_id,
            page_size: -1,
            page_token: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn list_drafts_pages_through_an_account_exactly_once() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let mut expected = Vec::new();
    for n in 0..5 {
        expected.push(
            client
                .create_draft(CreateDraftRequest {
                    subject: format!("draft {n}"),
                    ..server.create_request()
                })
                .await
                .unwrap()
                .into_inner()
                .id,
        );
    }
    expected.sort_unstable();

    let mut seen = Vec::new();
    let mut token = String::new();
    for _ in 0..10 {
        let page = client
            .list_drafts(ListDraftsRequest {
                account_id: server.account_id,
                page_size: 2,
                page_token: token.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(page.drafts.len() <= 2);
        seen.extend(page.drafts.iter().map(|d| d.id));
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    seen.sort_unstable();
    assert_eq!(seen, expected, "paging repeated or skipped a draft");

    server.stop().await;
}

#[tokio::test]
async fn a_draft_page_token_cannot_be_re_aimed_at_another_account() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    for n in 0..3 {
        client
            .create_draft(CreateDraftRequest {
                subject: format!("draft {n}"),
                ..server.create_request()
            })
            .await
            .unwrap();
    }

    let token = client
        .list_drafts(ListDraftsRequest {
            account_id: server.account_id,
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .next_page_token;
    assert!(!token.is_empty(), "a full page should carry a token");

    let status = client
        .list_drafts(ListDraftsRequest {
            account_id: server.account_id + 1,
            page_size: 1,
            page_token: token,
        })
        .await
        .expect_err("a token from another account must be refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn render_returns_a_message_that_parses_back_into_the_draft() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created = client
        .create_draft(CreateDraftRequest {
            from: Some(address("alice@example.com", "Café Ünicode")),
            to: vec![address("bob@example.net", "Bob")],
            cc: vec![address("carol@example.org", "")],
            bcc: vec![address("secret@example.org", "")],
            subject: "Rapport für Q3 — résumé".to_owned(),
            body_text: "Le rapport est joint.".to_owned(),
            body_html: Some("<p>Le rapport est <i>joint</i>.</p>".to_owned()),
            attachments: vec![NewDraftAttachment {
                filename: "rapport.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                content: b"%PDF-1.7\ntrailer\n".to_vec(),
            }],
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    let rendered = client
        .render_draft(RenderDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap()
        .into_inner();

    let parsed = parse_message(&rendered.mime);
    assert_eq!(parsed.subject.as_deref(), Some("Rapport für Q3 — résumé"));
    assert_eq!(parsed.from_addr.as_deref(), Some("alice@example.com"));
    assert_eq!(parsed.from_name.as_deref(), Some("Café Ünicode"));
    assert_eq!(parsed.to_addrs.as_deref(), Some("bob@example.net"));
    assert_eq!(parsed.cc_addrs.as_deref(), Some("carol@example.org"));
    assert_eq!(
        parsed.body_text.as_deref().map(str::trim_end),
        Some("Le rapport est joint.")
    );
    assert_eq!(
        parsed.body_html.as_deref().map(str::trim_end),
        Some("<p>Le rapport est <i>joint</i>.</p>")
    );
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(
        parsed.attachments[0].filename.as_deref(),
        Some("rapport.pdf")
    );
    assert_eq!(
        parsed.message_id.as_deref(),
        Some(rendered.message_id.as_str())
    );

    // Bcc reaches the envelope and only the envelope.
    assert_eq!(
        rendered.envelope_recipients,
        vec![
            "bob@example.net".to_owned(),
            "carol@example.org".to_owned(),
            "secret@example.org".to_owned()
        ]
    );
    assert!(!String::from_utf8_lossy(&rendered.mime).contains("secret@example.org"));

    server.stop().await;
}

#[tokio::test]
async fn a_reply_renders_with_the_parents_threading_headers() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let parent = server
        .message("parent@example.com", Some("root@example.com"))
        .await;

    let created = client
        .create_draft(CreateDraftRequest {
            in_reply_to_message_id: Some(parent),
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(created.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        created.references,
        vec![
            "root@example.com".to_owned(),
            "parent@example.com".to_owned()
        ]
    );

    let rendered = client
        .render_draft(RenderDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap()
        .into_inner();
    let parsed = parse_message(&rendered.mime);
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        parsed.references.as_deref(),
        Some("root@example.com parent@example.com")
    );

    server.stop().await;
}

#[tokio::test]
async fn rendering_uses_crlf_throughout_and_respects_the_line_limit() {
    // The two properties an SMTP server enforces and a local test would
    // otherwise never notice.
    let server = TestServer::start().await;
    let created = server
        .client()
        .await
        .create_draft(CreateDraftRequest {
            subject: "Ünicode ".repeat(30).trim_end().to_owned(),
            body_text: format!("Café\n{}\nplain\r\n", "x".repeat(3000)),
            to: (0..10)
                .map(|n| {
                    address(
                        &format!("recipient-{n}@a-fairly-long-domain.example.com"),
                        "",
                    )
                })
                .collect(),
            ..server.create_request()
        })
        .await
        .unwrap()
        .into_inner();

    let rendered = server
        .client()
        .await
        .render_draft(RenderDraftRequest {
            draft_id: created.id,
        })
        .await
        .unwrap()
        .into_inner()
        .mime;

    for (index, byte) in rendered.iter().enumerate() {
        if *byte == b'\n' {
            assert_eq!(rendered.get(index.wrapping_sub(1)), Some(&b'\r'), "bare LF");
        }
    }
    for line in rendered.split(|&b| b == b'\n') {
        let len = line.strip_suffix(b"\r").unwrap_or(line).len();
        assert!(len <= 998, "line of {len} octets exceeds RFC 5322's limit");
    }

    server.stop().await;
}
