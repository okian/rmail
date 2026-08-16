//! Integration test: drive `SearchService.SearchAttachments` end-to-end
//! against an in-process tonic server booted through the **real** daemon
//! wiring (`rmaild::serve_uds_injected`).
//!
//! # No provider is injected, and that is the point
//!
//! `Injected::ai_provider` is `None` here, so the daemon boots with
//! `ai_active = false` and `ai_service::NullProvider`. Attachment search still
//! answers, which is the observable form of the claim its scope row rests on:
//! nothing in `attach::search` can reach a model provider, so the RPC needs
//! `mail.read` and not `ai.invoke`. A test that injected a provider could not
//! tell the difference between "never called it" and "called it and it
//! happened not to matter".
//!
//! # Why the dense arm contributes nothing here
//!
//! `index.semantic.enabled = false`, the same call every other `rmaild` test
//! makes: `true` would have each test load — and on a cold cache download — an
//! ONNX model, and the deterministic hash fallback that replaces it produces
//! similarities that are stable but arbitrary. So `vec_chunks` is empty and
//! every assertion below rests on the lexical arm and on the page resolution,
//! both of which are deterministic. `rmail_core::attach::search`'s own tests
//! cover the fusion arithmetic directly, against hand-computed values.
//!
//! Every name here starts with `attach_search_` so a bare positional nextest
//! filter selects them: nextest matches such a filter against a test's *name*,
//! not against its binary id.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::attach::extract_attachments;
use rmail_core::config::IndexExtractConfig;
use rmail_core::repo;
use rmail_core::{Config, Database};
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{AttachmentHit, SearchAttachmentsRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

mod attach_fixture;
use attach_fixture::{message_with, pdf_bytes};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    inbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.ai.batching.enabled = false;
    config
}

impl TestServer {
    async fn start() -> Self {
        let config = base_config();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-atsearch-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-atsearch-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let (account_id, inbox_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id))
            })
            .unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            let events = rmail_core::events::EventLog::new(
                server_db.clone(),
                rmail_core::events::Retention::unlimited(),
            );
            let engine = rmail_core::sync::SyncEngine::new(
                server_db.clone(),
                events,
                rmail_core::sync::SyncOptions::default(),
            );
            let mail_store = rmail_core::mail::MailStore::new(
                server_db.clone(),
                engine.events().clone(),
                Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                    server_db.clone(),
                )),
            );
            let tag_store = rmail_core::tags::TagStore::new(
                server_db.clone(),
                Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                    server_db.clone(),
                )),
                config.tags.clone(),
            );
            rmaild::serve_uds_injected(
                &server_socket,
                server_db,
                engine,
                mail_store,
                tag_store,
                &config,
                rmaild::Injected {
                    // Deliberately absent — see the module docs.
                    ai_provider: None,
                    reranker: None,
                    ..Default::default()
                },
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
            account_id,
            inbox_id,
            next_uid: std::cell::Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> SearchServiceClient<Channel> {
        SearchServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn mailbox(&self, name: &str) -> i64 {
        let account_id = self.account_id;
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Seed a message carrying `attachments` and run the real extraction
    /// pipeline over it — the same call the indexer makes, so the lexical
    /// index this test queries is written by the code that writes it in
    /// production.
    async fn index(&self, mailbox_id: i64, attachments: &[(&str, &str, &[u8])]) -> i64 {
        let raw = message_with(attachments);
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, mailbox_id);
        let message_id = self
            .db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("With attachments".to_owned()),
                        from_addr: Some("ada@example.com".to_owned()),
                        raw: Some(raw),
                        date: Some(1_700_000_000 + uid),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let meta: Vec<(String, String, String, i64)> = attachments
            .iter()
            .enumerate()
            .map(|(index, (filename, content_type, bytes))| {
                (
                    index.to_string(),
                    (*filename).to_owned(),
                    (*content_type).to_owned(),
                    bytes.len() as i64,
                )
            })
            .collect();
        self.db
            .write(move |c| {
                for (part_id, filename, content_type, size) in &meta {
                    c.execute(
                        "INSERT INTO attachments
                             (message_id, part_id, filename, content_type, size)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![message_id, part_id, filename, content_type, size],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        extract_attachments(&self.db, &IndexExtractConfig::default(), message_id)
            .await
            .unwrap();
        message_id
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

async fn search(server: &TestServer, req: SearchAttachmentsRequest) -> Vec<AttachmentHit> {
    server
        .client()
        .await
        .search_attachments(req)
        .await
        .expect("SearchAttachments RPC")
        .into_inner()
        .hits
}

fn ask(query: &str) -> SearchAttachmentsRequest {
    SearchAttachmentsRequest {
        query: query.to_owned(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The acceptance criterion, over the real daemon: a clause query returns the
/// exact attachment and the page it is on.
#[tokio::test]
async fn attach_search_returns_the_exact_attachment_and_page() {
    let server = TestServer::start().await;
    let contract = server
        .index(
            server.inbox_id,
            &[
                (
                    "logo.txt",
                    "text/plain",
                    b"Acme Corporation brand assets and colour palette." as &[u8],
                ),
                (
                    "contract.pdf",
                    "application/pdf",
                    &pdf_bytes(&[
                        "Recitals and definitions for the parties to this agreement",
                        "Either party may terminate this agreement for convenience on thirty \
                         days written notice",
                        "Signatures and counterparts executed by the parties hereto",
                    ]),
                ),
            ],
        )
        .await;

    let hits = search(&server, ask("\"terminate this agreement for convenience\"")).await;

    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(hit.message_id, contract);
    // Not merely "the message that carried it" — the attachment itself, and
    // not the sibling in the same message.
    assert_eq!(hit.part_id, "1");
    assert_eq!(hit.filename, "contract.pdf");
    assert_eq!(hit.content_type, "application/pdf");
    assert_eq!(hit.mailbox, "INBOX");
    assert_eq!(hit.account_id, server.account_id);
    assert_eq!(hit.pages, Some(3));
    assert_eq!(hit.page, Some(2), "the clause is on page two: {hit:?}");
    assert_eq!(hit.provenance, "native");
    assert_eq!(hit.lexical_rank, Some(1));
    assert!(hit.score > 0.0);
    assert!(
        hit.excerpt.to_lowercase().contains("convenience"),
        "excerpt was {:?}",
        hit.excerpt
    );
    server.stop().await;
}

/// Two attachments in *different* messages both match; both come back, each
/// naming its own part.
#[tokio::test]
async fn attach_search_ranks_across_messages_and_names_each_part() {
    let server = TestServer::start().await;
    let first = server
        .index(
            server.inbox_id,
            &[(
                "msa.txt",
                "text/plain",
                b"Termination for convenience is available to either party." as &[u8],
            )],
        )
        .await;
    let second = server
        .index(
            server.inbox_id,
            &[(
                "sow.txt",
                "text/plain",
                b"Termination for convenience requires sixty days notice." as &[u8],
            )],
        )
        .await;

    let hits = search(&server, ask("termination convenience")).await;
    assert_eq!(hits.len(), 2, "{hits:?}");
    let mut found: Vec<i64> = hits.iter().map(|hit| hit.message_id).collect();
    found.sort_unstable();
    assert_eq!(found, vec![first, second]);
    for hit in &hits {
        assert_eq!(hit.part_id, "0");
        assert!(!hit.filename.is_empty());
        // Unset, because only PDF extraction and the OCR path record page
        // spans — which is exactly why the byte span travels alongside the
        // page rather than instead of it.
        assert_eq!(hit.page, None, "{hit:?}");
        assert!(hit.span_end >= hit.span_start);
    }
    server.stop().await;
}

/// The scoping knob a client uses to search inside the message on screen.
#[tokio::test]
async fn attach_search_can_be_scoped_to_one_message() {
    let server = TestServer::start().await;
    let first = server
        .index(
            server.inbox_id,
            &[(
                "a.txt",
                "text/plain",
                b"Termination for convenience, first copy." as &[u8],
            )],
        )
        .await;
    server
        .index(
            server.inbox_id,
            &[(
                "b.txt",
                "text/plain",
                b"Termination for convenience, second copy." as &[u8],
            )],
        )
        .await;

    let hits = search(
        &server,
        SearchAttachmentsRequest {
            query: "termination".to_owned(),
            message_id: first,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, first);
    server.stop().await;
}

/// Attachment text in another *mailbox* is still ordinary local mail — the
/// AI policy governs what leaves the host, not what a local index returns —
/// but an account filter is honored.
#[tokio::test]
async fn attach_search_honors_the_account_filter() {
    let server = TestServer::start().await;
    let archive = server.mailbox("Archive").await;
    server
        .index(
            archive,
            &[(
                "old.txt",
                "text/plain",
                b"Termination for convenience in the archived agreement." as &[u8],
            )],
        )
        .await;

    assert_eq!(search(&server, ask("termination")).await.len(), 1);
    let other = search(
        &server,
        SearchAttachmentsRequest {
            query: "termination".to_owned(),
            account_id: server.account_id + 999,
            ..Default::default()
        },
    )
    .await;
    assert!(other.is_empty(), "another account's mail leaked: {other:?}");
    server.stop().await;
}

/// A query nothing matches is an empty page, not an error — the ordinary
/// outcome a client renders as "no results".
#[tokio::test]
async fn attach_search_returns_an_empty_page_when_nothing_matches() {
    let server = TestServer::start().await;
    server
        .index(
            server.inbox_id,
            &[(
                "a.txt",
                "text/plain",
                b"Termination for convenience." as &[u8],
            )],
        )
        .await;
    assert!(search(&server, ask("zzzqqxjunobtainium")).await.is_empty());
    server.stop().await;
}

/// The boundary maps a domain error to the right code.
#[tokio::test]
async fn attach_search_rejects_an_empty_query() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .search_attachments(ask("   "))
        .await
        .expect_err("an empty query is not a query");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    server.stop().await;
}

/// A page is bounded whatever a caller asks for.
#[tokio::test]
async fn attach_search_clamps_an_absurd_limit() {
    let server = TestServer::start().await;
    server
        .index(
            server.inbox_id,
            &[(
                "a.txt",
                "text/plain",
                b"Termination for convenience." as &[u8],
            )],
        )
        .await;
    let hits = search(
        &server,
        SearchAttachmentsRequest {
            query: "termination".to_owned(),
            limit: u32::MAX,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(hits.len(), 1);
    server.stop().await;
}
