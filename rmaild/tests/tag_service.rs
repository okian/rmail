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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::{ChatRequest, ChatResponse, Provider, ProviderStream, StopReason, Usage};
use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::tags::TagStore;
use rmail_core::Error;
use rmail_proto::v1::tag_service_client::TagServiceClient;
use rmail_proto::v1::{
    bulk_tag_request, target, AddTagRequest, BulkTagRequest, CreateTagRequest, ListTagRulesRequest,
    ListTagsRequest, MessageIds, RemoveTagRequest, ResolveSuggestionRequest, SetTagRuleRequest,
    SuggestTagsRequest, TagRuleMode as ProtoTagRuleMode, TagSource, TagState,
    TagSyncMode as ProtoTagSyncMode, Target,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
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
// A scriptable provider for the on-demand `SuggestTags` path (task 57).
// Running out of scripted replies is an error rather than a default answer, so
// an unexpected extra call fails a test loudly instead of quietly succeeding —
// which is exactly how "we called the model for mail we should have skipped"
// would otherwise hide.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MockProvider {
    completions: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}

impl MockProvider {
    /// Script one classifier answer from `(tag, confidence)` pairs.
    fn queue_suggestions(&self, items: &[(&str, f64)]) {
        let suggestions: Vec<serde_json::Value> = items
            .iter()
            .map(|(tag, confidence)| {
                serde_json::json!({
                    "tag": tag,
                    "confidence": confidence,
                    "rationale": format!("because of {tag}"),
                })
            })
            .collect();
        self.completions
            .lock()
            .unwrap()
            .push_back(serde_json::json!({ "suggestions": suggestions }).to_string());
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.completions.lock().unwrap().pop_front() {
            Some(text) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: "mock-model".to_owned(),
                stop_reason: StopReason::EndTurn,
                text,
                usage: Usage::default(),
            }),
            None => Err(Error::unavailable(
                "mock provider: no scripted reply".to_owned(),
            )),
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::unavailable(
            "mock provider: streaming is not scripted".to_owned(),
        ))
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
        Self::build(imap, None).await
    }

    /// A daemon whose AI subsystem is active, backed by a scripted provider —
    /// what task 57's on-demand `SuggestTags` needs. Every other test in this
    /// suite runs without one, which is what keeps them exercising the
    /// pending-only replay path.
    async fn with_provider(provider: Arc<MockProvider>) -> Self {
        Self::build(FakeImap::default(), Some(provider)).await
    }

    async fn build(imap: FakeImap, provider: Option<Arc<MockProvider>>) -> Self {
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
            // The background pass would otherwise race every assertion in
            // this suite: the AI dispatch loop replays `NewMail` events and
            // would enqueue a `suggest_tags` job for each seeded message,
            // spending the provider script these tests hand to the *on-demand*
            // path. The RPC path is what this suite proves; the queued pass is
            // proven in `rmail_core::tags::ai`.
            config.tags.ai.suggest_on_new_mail = false;
            // Without an injected provider the daemon would still count its
            // AI subsystem as active (`ai.enabled` defaults on, and building
            // a client does not validate the key), which since task 57 means
            // `SuggestTags` tries a real network call. Every test that does
            // not script a provider is about proto translation, not the
            // classifier, so those get AI switched off outright and keep the
            // pending-only replay contract.
            config.ai.enabled = provider.is_some();
            let injected = rmaild::Injected {
                ai_provider: provider.map(|p| p as Arc<dyn Provider>),
                reranker: None,
            };
            rmaild::serve_uds_injected(
                &server_socket,
                server_db,
                engine,
                mail_store,
                tag_store,
                &config,
                injected,
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

// ---------------------------------------------------------------------------
// The on-demand classifier behind `SuggestTags` (task 57)
// ---------------------------------------------------------------------------

/// The acceptance criterion's "`SuggestTags` streams as Claude responds": with
/// an active AI subsystem the RPC classifies the message and streams each new
/// suggestion as it is written, rather than replaying only what a background
/// pass happened to leave behind.
#[tokio::test]
async fn suggest_tags_classifies_on_demand_and_streams_each_suggestion() {
    let provider = Arc::new(MockProvider::default());
    // With no `tag_rules` row nothing auto-applies at any confidence, so both
    // land pending and both are streamed.
    provider.queue_suggestions(&[("finance/invoice", 0.91), ("work", 0.62)]);
    let server = TestServer::with_provider(Arc::clone(&provider)).await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let mut got: Vec<(String, f64, String)> = Vec::new();
    while let Some(item) = stream.next().await {
        let s = item.unwrap();
        got.push((
            s.tag.as_ref().unwrap().name.clone(),
            s.confidence,
            s.rationale,
        ));
    }

    assert_eq!(provider.calls(), 1, "exactly one model call");
    assert_eq!(got.len(), 2, "both suggestions streamed: {got:?}");
    // Best first, and the confidence and rationale survive the round trip.
    assert_eq!(got[0].0, "finance/invoice");
    assert!((got[0].1 - 0.91).abs() < 1e-9);
    assert_eq!(got[0].2, "because of finance/invoice");
    assert_eq!(got[1].0, "work");

    // ...and they are durable pending rows, not stream-only artifacts. The
    // second read replays them and — the cost control — does *not* classify
    // again, which is why the exhausted script never errors.
    let mut again = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let mut replayed: Vec<String> = Vec::new();
    while let Some(item) = again.next().await {
        replayed.push(item.unwrap().tag.unwrap().name);
    }
    replayed.sort();
    assert_eq!(replayed, ["finance/invoice", "work"]);
    assert_eq!(
        provider.calls(),
        1,
        "a message with unanswered suggestions must not be classified twice"
    );

    server.stop().await;
}

/// The cost control prd.md names outright: "skip already-user-tagged mail".
/// The provider is scripted with nothing at all, so any call would be an
/// error — which is the point, since a silent extra call is exactly the
/// failure this guards.
#[tokio::test]
async fn suggest_tags_never_calls_a_model_for_mail_the_recipient_already_tagged() {
    let provider = Arc::new(MockProvider::default());
    let server = TestServer::with_provider(Arc::clone(&provider)).await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    client
        .add_tag(AddTagRequest {
            target: Some(Target {
                of: Some(target::Of::MessageId(message_id)),
            }),
            names: vec!["work".to_owned()],
        })
        .await
        .unwrap();

    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let mut count = 0;
    while let Some(item) = stream.next().await {
        item.unwrap();
        count += 1;
    }

    assert_eq!(count, 0, "a filed message yields no suggestions");
    assert_eq!(
        provider.calls(),
        0,
        "and must not have reached a provider at all"
    );

    server.stop().await;
}

/// The error path: a provider failure must reach the client as a `Status` on
/// the stream, not as a silently truncated success. `SuggestTags` opens its
/// response before the call is made, so this is the only way the failure can
/// be reported at all — and a stream that just ends looks exactly like "no
/// suggestions", which is the wrong answer to show somebody.
#[tokio::test]
async fn a_provider_failure_reaches_the_client_as_a_stream_status() {
    // Nothing scripted: `MockProvider` answers `Unavailable`.
    let provider = Arc::new(MockProvider::default());
    let server = TestServer::with_provider(Arc::clone(&provider)).await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let status = stream
        .next()
        .await
        .expect("the failure is reported, not swallowed")
        .expect_err("expected a Status");

    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(provider.calls(), 1, "the call really was attempted");
    assert!(stream.next().await.is_none(), "and the stream then ends");

    server.stop().await;
}

/// A message the background pass has already left suggestions on is replayed,
/// not re-classified — the second half of the cost control. Nothing is
/// scripted, so a model call here would surface as a stream error.
#[tokio::test]
async fn suggest_tags_replays_unanswered_suggestions_without_classifying_again() {
    let provider = Arc::new(MockProvider::default());
    let server = TestServer::with_provider(Arc::clone(&provider)).await;
    let (account_id, mailbox_id) = server.seed_account();
    let message_id = server.seed_message(account_id, mailbox_id, 1);
    let mut client = server.client().await;

    let tag = client
        .create_tag(CreateTagRequest {
            account_id,
            name: "travel".to_owned(),
            color: None,
            sync_mode: Some(ProtoTagSyncMode::Local as i32),
            parent_id: None,
        })
        .await
        .unwrap()
        .into_inner();
    let store = TagStore::new(
        server.db.clone(),
        Arc::new(FakeImap::default()) as Arc<dyn ImapMutator>,
        rmail_core::config::TagsConfig::default(),
    );
    store
        .record_suggestion(
            tag.id,
            rmail_core::tags::Target::Message(message_id),
            0.7,
            "an itinerary".to_owned(),
        )
        .await
        .unwrap();

    let mut stream = client
        .suggest_tags(SuggestTagsRequest { message_id })
        .await
        .unwrap()
        .into_inner();
    let first = stream
        .next()
        .await
        .expect("the pending row arrives first")
        .unwrap();
    assert_eq!(first.tag.as_ref().unwrap().name, "travel");
    assert!(
        stream.next().await.is_none(),
        "nothing follows: the message was replayed, not re-classified"
    );
    assert_eq!(provider.calls(), 0, "and no model call was made");

    server.stop().await;
}

/// An operator can turn on auto-apply without hand-written SQL.
///
/// # Why this test exists
///
/// Task 57 shipped the whole auto-apply mechanism — `tag_rules`, the
/// confidence floor, the `mode = auto` branch — and tested it inside
/// `rmail-core`, but exposed no way to *write* a rule. `TagStore::set_tag_rule`
/// had no caller outside the crate, so in a running daemon every suggestion
/// pended forever and the tested code path was unreachable.
///
/// That is the same shape as this project's worst prior defect: tasks 16-21
/// shipped an index pipeline nothing in the daemon ever enqueued work into,
/// and each of those tasks passed its own gate. A unit test proves a mechanism
/// works; only a test that goes through the daemon proves it is *reachable*.
#[tokio::test]
async fn a_tag_rule_can_be_created_and_read_back_through_the_daemon() {
    let server = TestServer::start().await;
    let (account_id, _mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    // Nothing configured: the listing says so rather than being silently
    // empty, because "no rules" and "auto-apply is off" are the same state.
    let empty = client
        .list_tag_rules(ListTagRulesRequest { account_id })
        .await
        .unwrap()
        .into_inner();
    assert!(empty.rules.is_empty());

    let rule = client
        .set_tag_rule(SetTagRuleRequest {
            account_id,
            name: "invoices".to_owned(),
            tag_name: "finance/invoices".to_owned(),
            mode: ProtoTagRuleMode::Auto as i32,
            min_conf: 0.95,
            enabled: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rule.name, "invoices");
    assert_eq!(rule.tag_name, "finance/invoices");
    assert_eq!(rule.mode, ProtoTagRuleMode::Auto as i32);
    assert!((rule.min_conf - 0.95).abs() < f64::EPSILON);
    assert!(rule.enabled);

    let listed = client
        .list_tag_rules(ListTagRulesRequest { account_id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.rules.len(), 1);
    assert_eq!(listed.rules[0].id, rule.id);
    assert_eq!(listed.rules[0].mode, ProtoTagRuleMode::Auto as i32);

    // Upsert on (account, name): the same name re-points rather than
    // accumulating a second rule the operator cannot see the effect of.
    let repointed = client
        .set_tag_rule(SetTagRuleRequest {
            account_id,
            name: "invoices".to_owned(),
            tag_name: "finance/invoices".to_owned(),
            mode: ProtoTagRuleMode::Suggest as i32,
            min_conf: 0.5,
            enabled: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(repointed.id, rule.id, "same row, not a second one");
    assert_eq!(repointed.mode, ProtoTagRuleMode::Suggest as i32);
    assert!(!repointed.enabled);
    let listed = client
        .list_tag_rules(ListTagRulesRequest { account_id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.rules.len(), 1);

    server.stop().await;
}

/// An unspecified mode must not be read as `auto`.
///
/// `TAG_RULE_MODE_UNSPECIFIED` is proto3's zero value, so an older client, a
/// hand-built request, or a field the caller simply did not set all arrive
/// here as 0. Applying tags without anyone confirming them is the privileged
/// half of this feature; defaulting to it would hand that privilege to a
/// caller who never asked for it.
#[tokio::test]
async fn an_unspecified_rule_mode_is_suggest_not_auto() {
    let server = TestServer::start().await;
    let (account_id, _mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    let rule = client
        .set_tag_rule(SetTagRuleRequest {
            account_id,
            name: "unset".to_owned(),
            tag_name: "misc".to_owned(),
            mode: ProtoTagRuleMode::Unspecified as i32,
            min_conf: 0.9,
            enabled: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rule.mode, ProtoTagRuleMode::Suggest as i32);

    server.stop().await;
}

/// A confidence floor outside 0.0..=1.0 is refused rather than stored.
#[tokio::test]
async fn an_out_of_range_confidence_floor_is_invalid_argument() {
    let server = TestServer::start().await;
    let (account_id, _mailbox_id) = server.seed_account();
    let mut client = server.client().await;

    for min_conf in [-0.1, 1.5, f64::NAN] {
        let status = client
            .set_tag_rule(SetTagRuleRequest {
                account_id,
                name: "bad".to_owned(),
                tag_name: "misc".to_owned(),
                mode: ProtoTagRuleMode::Auto as i32,
                min_conf,
                enabled: true,
            })
            .await
            .expect_err("min_conf {min_conf} must be refused");
        assert_eq!(status.code(), Code::InvalidArgument, "min_conf {min_conf}");
    }

    server.stop().await;
}
