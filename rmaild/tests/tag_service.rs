//! Integration test: drive `TagService` end-to-end against an in-process
//! tonic server over a Unix domain socket.
//!
//! `TagService`'s domain logic (hierarchy/cycle rejection, the IMAP
//! keyword/Gmail-label round-trip, the `auto` downgrade driven by a real
//! IMAP `NO`, coalesced bulk `STORE`) is already proven against a real
//! [`rmail_core::imap::mock`] server in `rmail-core`'s own `tags::sync`
//! tests (see that module's docs — the same "a live server cannot be dialed
//! in-process" reason `mail_service.rs`'s own suite gives for using a fake
//! mutator here instead). What this suite proves is everything specific to
//! the gRPC surface: proto <-> domain translation, the request/response
//! shapes, streaming `SuggestTags`, and that `TagService` really is wired
//! into the daemon and reachable over the wire.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::tags::TagStore;
use rmail_core::Error;
use rmail_proto::v1::tag_service_client::TagServiceClient;
use rmail_proto::v1::{
    bulk_tag_request, target, AddTagRequest, BulkTagRequest, CreateTagRequest, ListTagsRequest,
    MessageIds, RemoveTagRequest, ResolveSuggestionRequest, SuggestTagsRequest, TagSource,
    TagState, TagSyncMode as ProtoTagSyncMode, Target,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// A fake IMAP mutator: records every `store_keyword` call and can be told to
// fail it on demand — see the module docs on why the real wire-level proof
// lives in `rmail-core` instead.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct StoreCall {
    mailbox: String,
    uids: Vec<i64>,
    keyword: String,
    add: bool,
}

#[derive(Debug, Default)]
struct FakeImap {
    calls: Mutex<Vec<StoreCall>>,
    fail_store: bool,
}

impl FakeImap {
    fn calls(&self) -> Vec<StoreCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ImapMutator for FakeImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Err(Error::internal("not exercised by this suite"))
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal("not exercised by this suite"))
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal("not exercised by this suite"))
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Err(Error::internal("not exercised by this suite"))
    }
    async fn store_keyword(
        &self,
        _account_id: i64,
        mailbox: &str,
        _uidvalidity: i64,
        uids: &[i64],
        keyword: &str,
        _prefer_gmail_label: bool,
        add: bool,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(StoreCall {
            mailbox: mailbox.to_owned(),
            uids: uids.to_vec(),
            keyword: keyword.to_owned(),
            add,
        });
        if self.fail_store {
            return Err(Error::unavailable("fake imap: store refused"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test server harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    imap: Arc<FakeImap>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::with_imap(FakeImap::default()).await
    }

    async fn with_imap(imap: FakeImap) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-tag-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-tag-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), Retention::unlimited());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let mail_store = MailStore::new(
            db.clone(),
            log,
            Arc::new(FakeImap::default()) as Arc<dyn ImapMutator>,
        );
        let imap = Arc::new(imap);
        let tag_store = TagStore::new(
            db.clone(),
            imap.clone() as Arc<dyn ImapMutator>,
            rmail_core::config::TagsConfig::default(),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            rmaild::serve_uds_with_stores(
                &server_socket,
                server_db,
                engine,
                mail_store,
                tag_store,
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
            imap,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> TagServiceClient<Channel> {
        TagServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    fn imap_calls(&self) -> Vec<StoreCall> {
        self.imap.calls()
    }

    /// A fresh account with an INBOX mailbox. Returns `(account_id,
    /// mailbox_id)`.
    fn seed_account(&self) -> (i64, i64) {
        let account_id = self
            .db
            .with_write(|c| {
                repo::insert_account(
                    c,
                    &NewAccount {
                        name: format!("acct-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let mailbox_id = self
            .db
            .with_write(move |c| {
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
        (account_id, mailbox_id)
    }

    fn seed_message(&self, account_id: i64, mailbox_id: i64, uid: i64) -> i64 {
        self.db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
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

fn message_target(id: i64) -> Target {
    Target {
        of: Some(target::Of::MessageId(id)),
    }
}

// ---------------------------------------------------------------------------
// CreateTag / ListTags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_tag_then_list_tags_round_trips() {
    let server = TestServer::start().await;
    let (account_id, _mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    let tag = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "project/alpha".to_owned(),
            color: Some("#7aa2f7".to_owned()),
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tag.name, "project/alpha");
    assert_eq!(tag.color.as_deref(), Some("#7aa2f7"));
    assert!(tag.parent_id.is_some(), "auto-vivified under `project`");

    let listed = client
        .list_tags(ListTagsRequest { account_id })
        .await
        .unwrap()
        .into_inner()
        .tags;
    // `project` (auto-created ancestor) + `project/alpha`.
    assert_eq!(listed.len(), 2);
    assert!(listed
        .iter()
        .any(|t| t.tag.as_ref().unwrap().name == "project/alpha"));

    server.stop().await;
}

#[tokio::test]
async fn create_tag_rejects_a_hierarchy_cycle() {
    let server = TestServer::start().await;
    let (account_id, _mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    let a = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "a".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: None,
        })
        .await
        .unwrap()
        .into_inner();
    let b = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "b".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: Some(a.id),
        })
        .await
        .unwrap()
        .into_inner();

    let status = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "a".to_owned(),
            color: None,
            sync_mode: None,
            parent_id: Some(b.id),
        })
        .await
        .expect_err("reparenting a under its own child must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// AddTag / RemoveTag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_tag_creates_on_demand_and_remove_tag_removes_it() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let response = client
        .add_tag(AddTagRequest {
            target: Some(message_target(message_id)),
            names: vec!["work".to_owned(), "urgent".to_owned()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.applications.len(), 2);
    for application in &response.applications {
        assert_eq!(application.source(), TagSource::User);
    }

    client
        .remove_tag(RemoveTagRequest {
            target: Some(message_target(message_id)),
            names: vec!["work".to_owned()],
        })
        .await
        .unwrap();

    let tags = client
        .list_tags(ListTagsRequest { account_id })
        .await
        .unwrap()
        .into_inner()
        .tags;
    let work = tags
        .iter()
        .find(|t| t.tag.as_ref().unwrap().name == "work")
        .unwrap();
    assert_eq!(work.message_count, 0);
    let urgent = tags
        .iter()
        .find(|t| t.tag.as_ref().unwrap().name == "urgent")
        .unwrap();
    assert_eq!(urgent.message_count, 1);

    server.stop().await;
}

#[tokio::test]
async fn add_tag_without_a_target_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .add_tag(AddTagRequest {
            target: None,
            names: vec!["work".to_owned()],
        })
        .await
        .expect_err("a missing target must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// BulkTag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_tag_by_message_ids_coalesces_into_one_store_call() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    client
        .create_tag(CreateTagRequest {
            account_id,
            name: "urgent".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Imap as i32),
            parent_id: None,
        })
        .await
        .unwrap();

    let ids = vec![
        server.seed_message(account_id, mailbox_id, 1),
        server.seed_message(account_id, mailbox_id, 2),
        server.seed_message(account_id, mailbox_id, 3),
    ];

    let response = client
        .bulk_tag(BulkTagRequest {
            account_id,
            names: vec!["urgent".to_owned()],
            selector: Some(bulk_tag_request::Selector::MessageIds(MessageIds {
                ids: ids.clone(),
            })),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.message_count, 3);
    assert_eq!(response.applied, 3);

    let calls = server.imap_calls();
    assert_eq!(
        calls.len(),
        1,
        "three messages sharing a mailbox must coalesce into one STORE call, got: {calls:?}"
    );
    let mut uids = calls[0].uids.clone();
    uids.sort_unstable();
    assert_eq!(uids, vec![1, 2, 3]);
    assert!(calls[0].add);

    server.stop().await;
}

#[tokio::test]
async fn bulk_tag_by_query_selects_matching_messages() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.seed_account();
    let matching = server
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 10,
                    uidvalidity: 1,
                    from_addr: Some("billing@stripe.com".to_owned()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let _other = server.seed_message(account_id, mailbox_id, 11);
    let mut client = server.client().await;

    let response = client
        .bulk_tag(BulkTagRequest {
            account_id,
            names: vec!["finance/receipt".to_owned()],
            selector: Some(bulk_tag_request::Selector::Query("from:stripe".to_owned())),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.message_count, 1);

    let tags = client
        .list_tags(ListTagsRequest { account_id })
        .await
        .unwrap()
        .into_inner()
        .tags;
    let receipt = tags
        .iter()
        .find(|t| t.tag.as_ref().unwrap().name == "finance/receipt")
        .unwrap();
    assert_eq!(receipt.message_count, 1);
    let _ = matching;

    server.stop().await;
}

// ---------------------------------------------------------------------------
// SuggestTags / ResolveSuggestion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suggest_tags_streams_pending_suggestions_and_resolve_accepts() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let tag = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "finance/invoice".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: None,
        })
        .await
        .unwrap()
        .into_inner();

    // Task 57's own job would write this; this suite writes it directly
    // (via the same `TagStore::record_suggestion` seam task 57 uses) to
    // prove `SuggestTags` streams *existing* pending rows without ever
    // calling a model itself.
    let suggestion_store = TagStore::new(
        server.db.clone(),
        Arc::new(FakeImap::default()) as Arc<dyn ImapMutator>,
        rmail_core::config::TagsConfig::default(),
    );
    suggestion_store
        .record_suggestion(
            tag.id,
            rmail_core::tags::Target::Message(message_id),
            0.83,
            "mentions an invoice number".to_owned(),
        )
        .await
        .unwrap();

    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let suggestion = stream
        .next()
        .await
        .expect("one pending suggestion")
        .unwrap();
    assert_eq!(suggestion.tag.as_ref().unwrap().name, "finance/invoice");
    assert!((suggestion.confidence - 0.83).abs() < 1e-9);
    assert_eq!(suggestion.rationale, "mentions an invoice number");
    assert!(stream.next().await.is_none());

    client
        .resolve_suggestion(ResolveSuggestionRequest {
            message_tag_id: suggestion.message_tag_id,
            accept: true,
        })
        .await
        .unwrap();

    // Now applied, not pending -- SuggestTags returns nothing further.
    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    assert!(stream.next().await.is_none());

    let tags = client
        .list_tags(ListTagsRequest { account_id })
        .await
        .unwrap()
        .into_inner()
        .tags;
    let invoice = tags
        .iter()
        .find(|t| t.tag.as_ref().unwrap().name == "finance/invoice")
        .unwrap();
    assert_eq!(invoice.message_count, 1);

    server.stop().await;
}

#[tokio::test]
async fn resolve_suggestion_twice_is_failed_precondition() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let tag = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "newsletter".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: None,
        })
        .await
        .unwrap()
        .into_inner();
    let suggestion_store = TagStore::new(
        server.db.clone(),
        Arc::new(FakeImap::default()) as Arc<dyn ImapMutator>,
        rmail_core::config::TagsConfig::default(),
    );
    let row_id = suggestion_store
        .record_suggestion(
            tag.id,
            rmail_core::tags::Target::Message(message_id),
            0.4,
            String::new(),
        )
        .await
        .unwrap()
        .unwrap();

    client
        .resolve_suggestion(ResolveSuggestionRequest {
            message_tag_id: row_id,
            accept: false,
        })
        .await
        .unwrap();

    let status = client
        .resolve_suggestion(ResolveSuggestionRequest {
            message_tag_id: row_id,
            accept: true,
        })
        .await
        .expect_err("an already-resolved suggestion must not resolve twice");
    assert_eq!(status.code(), Code::FailedPrecondition);

    server.stop().await;
}

/// Not directly observable through the state enum from the client alone,
/// but pinned here so an accidental future change to `TagState`'s wire
/// numbering is caught by a compile error rather than a silent
/// misinterpretation.
#[test]
fn tag_state_wire_values_are_stable() {
    assert_eq!(TagState::Applied as i32, 1);
    assert_eq!(TagState::Pending as i32, 2);
    assert_eq!(TagState::Rejected as i32, 3);
}
