//! Integration test: drive `SavedSearchService` end-to-end against an
//! in-process tonic server over a Unix domain socket, booted through the
//! real [`rmaild::serve_uds_with_config`] so the saved-search handler sits
//! next to the *same* `SearchApi` the daemon serves `SearchService` from.
//!
//! That co-location is the point of this file. The acceptance bullet task 35
//! owns says a saved search is "re-runnable through the full pipeline", and
//! the only way to prove that rather than assert it is to compare, over the
//! wire, `RunSavedSearch(name)` against `Search(<the query that name holds>)`
//! and require them to agree hit for hit. A handler that had quietly grown
//! its own retrieval path — or that had cached a result set — would differ
//! the moment either drifted.
//!
//! The other half is the smart folder's "membership stays live without
//! moving server mail": a message that arrives after the last evaluation
//! appears in `ListSmartFolderMembers` with no evaluation in between, and
//! `EvaluateSmartFolder` reports a delta rather than re-firing for members
//! it has already seen.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::index::fts::FtsIndex;
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::repo;
use rmail_core::{Config, Database};
use rmail_proto::v1::saved_search_service_client::SavedSearchServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{
    CreateSavedSearchRequest, CreateSmartFolderRequest, DeleteSavedSearchRequest,
    DeleteSmartFolderRequest, EvaluateSmartFolderRequest, ListSavedSearchesRequest,
    ListSmartFolderMembersRequest, ListSmartFoldersRequest, RunSavedSearchRequest, SearchHit,
    SearchRequest, UpdateSavedSearchRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous: a liveness bound on a spawned pipeline task, not a latency
/// measurement.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        // Semantic indexing off, matching `serve_uds`'s own convention: the
        // deterministic hash fallback keeps this file from downloading an
        // ONNX model to exercise a surface that does not need one.
        let mut config = Config::default();
        config.index.semantic.enabled = false;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-saved-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-saved-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);
        let db = Database::open(&db_path).unwrap();

        let (account_id, mailbox_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
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
            .unwrap();

        let fts = FtsIndex::new(db.clone(), config.search.bm25_weights.clone());
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, server_db, config, async move {
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
            fts,
            queue,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> SavedSearchServiceClient<Channel> {
        SavedSearchServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn search_client(&self) -> SearchServiceClient<Channel> {
        SearchServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// Insert, extract, and lexically index a message through the real
    /// pipeline — the established pattern in `rmaild/tests/search_service.rs`.
    async fn index(&self, from: &str, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            from_addr: Some(from.to_owned()),
            subject: Some(subject.to_owned()),
            body_text: Some(body.to_owned()),
            date: Some(1_700_000_000 + uid),
            ..Default::default()
        };
        let id = self
            .db
            .with_write(move |c| repo::insert_message(c, &new))
            .unwrap();
        extract_message(&self.db, &self.queue, id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(id).await.unwrap();
        id
    }

    /// Every `RULE_FIRED` event's `message_id`, read straight from the log.
    async fn rule_fired_message_ids(&self) -> Vec<i64> {
        self.db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT message_id FROM events WHERE kind = 'RULE_FIRED'
                     AND message_id IS NOT NULL ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(rows)
            })
            .await
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

/// Drain a stream to completion, failing rather than hanging.
async fn drain<S, T>(mut stream: S) -> Vec<T>
where
    S: tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(item))) => out.push(item),
            Ok(Some(Err(status))) => panic!("stream item was an error: {status}"),
            Ok(None) => return out,
            Err(_) => panic!("timed out draining a stream"),
        }
    }
}

fn hit_ids(hits: &[SearchHit]) -> Vec<i64> {
    hits.iter()
        .map(|hit| hit.message.as_ref().map_or(0, |m| m.id))
        .collect()
}

// ---------------------------------------------------------------------------
// A saved search re-runs through the full pipeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_saved_search_matches_a_live_search_of_the_same_query_hit_for_hit() {
    let server = TestServer::start().await;
    server
        .index(
            "billing@stripe.com",
            "Invoice 41",
            "quarterly invoice total",
        )
        .await;
    server
        .index(
            "billing@stripe.com",
            "Invoice 42",
            "quarterly invoice again",
        )
        .await;
    server
        .index("noreply@example.com", "Newsletter", "unrelated chatter")
        .await;

    let query = "from:stripe invoice";
    let mut client = server.client().await;
    client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
            query: query.to_owned(),
        })
        .await
        .expect("create");

    let saved = drain(
        client
            .run_saved_search(RunSavedSearchRequest {
                account_id: server.account_id,
                name: "weekly".to_owned(),
                ..Default::default()
            })
            .await
            .expect("run")
            .into_inner(),
    )
    .await;

    let live = drain(
        server
            .search_client()
            .await
            .search(SearchRequest {
                query: query.to_owned(),
                account_id: server.account_id,
                ..Default::default()
            })
            .await
            .expect("search")
            .into_inner(),
    )
    .await;

    assert!(!saved.is_empty(), "the saved search returned nothing");
    assert_eq!(
        hit_ids(&saved),
        hit_ids(&live),
        "a saved search must be the same pipeline call as a live search"
    );
    for (a, b) in saved.iter().zip(live.iter()) {
        assert!(
            (a.score - b.score).abs() < f64::EPSILON,
            "scores diverged: {} vs {}",
            a.score,
            b.score
        );
        assert_eq!(a.sources, b.sources);
    }
    server.stop().await;
}

#[tokio::test]
async fn a_saved_search_run_reflects_a_later_edit_rather_than_a_cached_result() {
    // The failure this guards against is a stored result set: an
    // implementation that snapshotted ids at create time would keep
    // returning the Stripe mail after the query was pointed elsewhere.
    let server = TestServer::start().await;
    let stripe = server
        .index("billing@stripe.com", "Invoice 41", "quarterly invoice")
        .await;
    let aws = server
        .index("billing@aws.com", "Statement", "quarterly statement")
        .await;

    let mut client = server.client().await;
    client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
            query: "from:stripe".to_owned(),
        })
        .await
        .expect("create");
    let before = drain(
        client
            .run_saved_search(RunSavedSearchRequest {
                account_id: server.account_id,
                name: "Weekly".to_owned(),
                ..Default::default()
            })
            .await
            .expect("run")
            .into_inner(),
    )
    .await;
    assert_eq!(hit_ids(&before), vec![stripe]);

    client
        .update_saved_search(UpdateSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
            query: "from:aws".to_owned(),
        })
        .await
        .expect("update");
    let after = drain(
        client
            .run_saved_search(RunSavedSearchRequest {
                account_id: server.account_id,
                name: "Weekly".to_owned(),
                ..Default::default()
            })
            .await
            .expect("run")
            .into_inner(),
    )
    .await;
    assert_eq!(hit_ids(&after), vec![aws]);
    server.stop().await;
}

#[tokio::test]
async fn saved_search_crud_round_trips_and_reports_its_error_paths() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created = client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
            query: "from:stripe -in:Spam".to_owned(),
        })
        .await
        .expect("create")
        .into_inner();
    assert_eq!(created.query, "from:stripe -in:Spam");
    assert_eq!(created.last_run_at, 0, "never run yet");

    let listed = client
        .list_saved_searches(ListSavedSearchesRequest {
            account_id: server.account_id,
        })
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.searches.len(), 1);
    assert_eq!(listed.searches[0].name, "Weekly");

    // Duplicate name.
    let status = client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
            query: "from:aws".to_owned(),
        })
        .await
        .expect_err("duplicate");
    assert_eq!(status.code(), Code::AlreadyExists);

    // A query with nothing to search for.
    let status = client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: server.account_id,
            name: "Empty".to_owned(),
            query: "   ".to_owned(),
        })
        .await
        .expect_err("unrunnable query");
    assert_eq!(status.code(), Code::InvalidArgument);

    // An account that does not exist.
    let status = client
        .create_saved_search(CreateSavedSearchRequest {
            account_id: 9_999,
            name: "Nowhere".to_owned(),
            query: "from:stripe".to_owned(),
        })
        .await
        .expect_err("missing account");
    assert_eq!(status.code(), Code::NotFound);

    // Running a name that does not exist.
    let status = client
        .run_saved_search(RunSavedSearchRequest {
            account_id: server.account_id,
            name: "nope".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("missing name");
    assert_eq!(status.code(), Code::NotFound);

    client
        .delete_saved_search(DeleteSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
        })
        .await
        .expect("delete");
    let status = client
        .delete_saved_search(DeleteSavedSearchRequest {
            account_id: server.account_id,
            name: "Weekly".to_owned(),
        })
        .await
        .expect_err("second delete");
    assert_eq!(status.code(), Code::NotFound);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Smart folders: a live view, evaluated without moving mail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smart_folder_membership_is_live_with_no_evaluation_in_between() {
    let server = TestServer::start().await;
    let first = server
        .index("billing@stripe.com", "Invoice 41", "quarterly invoice")
        .await;

    let mut client = server.client().await;
    client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
            predicate: "from:stripe".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create");

    let members = drain(
        client
            .list_smart_folder_members(ListSmartFolderMembersRequest {
                account_id: server.account_id,
                name: "Stripe".to_owned(),
                limit: 0,
            })
            .await
            .expect("members")
            .into_inner(),
    )
    .await;
    assert_eq!(
        members.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![first]
    );

    // Mail arrives. No Evaluate call, no sync, no re-index of the folder.
    let second = server
        .index(
            "receipts@stripe.com",
            "Receipt 9",
            "thanks for your payment",
        )
        .await;
    let members = drain(
        client
            .list_smart_folder_members(ListSmartFolderMembersRequest {
                account_id: server.account_id,
                name: "Stripe".to_owned(),
                limit: 0,
            })
            .await
            .expect("members")
            .into_inner(),
    )
    .await;
    assert_eq!(
        members.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![first, second],
        "membership must be live, not a stored copy"
    );
    // The mailbox itself is untouched: both messages are still in INBOX,
    // which is what "no mail is moved on the server" means locally too.
    for member in &members {
        assert_eq!(member.mailbox_id, server.mailbox_id);
    }

    // `limit` pages the view without evaluating anything.
    let first_only = drain(
        client
            .list_smart_folder_members(ListSmartFolderMembersRequest {
                account_id: server.account_id,
                name: "Stripe".to_owned(),
                limit: 1,
            })
            .await
            .expect("members")
            .into_inner(),
    )
    .await;
    assert_eq!(
        first_only.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![first]
    );
    server.stop().await;
}

#[tokio::test]
async fn two_saved_searches_running_at_once_do_not_truncate_each_other() {
    // `RunSavedSearch` reuses the search pipeline but deliberately not its
    // interactive generation slot: sharing it would let one run cancel the
    // other, and a cancelled pipeline stream *ends* rather than erroring, so
    // the loser would return a short page under a clean `OK`.
    let server = TestServer::start().await;
    for i in 0..4 {
        server
            .index(
                "billing@stripe.com",
                &format!("Invoice {i}"),
                "quarterly invoice total",
            )
            .await;
    }
    let mut client = server.client().await;
    for name in ["A", "B"] {
        client
            .create_saved_search(CreateSavedSearchRequest {
                account_id: server.account_id,
                name: name.to_owned(),
                query: "from:stripe".to_owned(),
            })
            .await
            .expect("create");
    }

    let solo = drain(
        client
            .run_saved_search(RunSavedSearchRequest {
                account_id: server.account_id,
                name: "A".to_owned(),
                ..Default::default()
            })
            .await
            .expect("run")
            .into_inner(),
    )
    .await;
    assert_eq!(solo.len(), 4, "baseline: the query matches four messages");

    let mut other = server.client().await;
    let a = client.run_saved_search(RunSavedSearchRequest {
        account_id: server.account_id,
        name: "A".to_owned(),
        ..Default::default()
    });
    let b = other.run_saved_search(RunSavedSearchRequest {
        account_id: server.account_id,
        name: "B".to_owned(),
        ..Default::default()
    });
    let (a, b) = tokio::join!(a, b);
    let (a, b) = tokio::join!(
        drain(a.expect("run A").into_inner()),
        drain(b.expect("run B").into_inner())
    );
    assert_eq!(hit_ids(&a), hit_ids(&solo), "run A was truncated");
    assert_eq!(hit_ids(&b), hit_ids(&solo), "run B was truncated");
    server.stop().await;
}

#[tokio::test]
async fn evaluate_smart_folder_notifies_once_for_a_new_member_and_never_again() {
    let server = TestServer::start().await;
    server
        .index("billing@stripe.com", "Invoice 41", "quarterly invoice")
        .await;

    let mut client = server.client().await;
    client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
            predicate: "from:stripe".to_owned(),
            notify: true,
            ..Default::default()
        })
        .await
        .expect("create");
    assert!(
        server.rule_fired_message_ids().await.is_empty(),
        "defining a folder must not notify for the backlog"
    );

    let newcomer = server
        .index(
            "receipts@stripe.com",
            "Receipt 9",
            "thanks for your payment",
        )
        .await;
    let first = client
        .evaluate_smart_folder(EvaluateSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
        })
        .await
        .expect("evaluate")
        .into_inner();
    assert_eq!(first.members, 2);
    assert_eq!(first.entered, vec![newcomer]);
    assert_eq!(first.notified, 1);

    for _ in 0..3 {
        let again = client
            .evaluate_smart_folder(EvaluateSmartFolderRequest {
                account_id: server.account_id,
                name: "Stripe".to_owned(),
            })
            .await
            .expect("evaluate")
            .into_inner();
        assert!(again.entered.is_empty());
        assert_eq!(again.notified, 0);
        assert_eq!(again.members, 2);
    }

    assert_eq!(
        server.rule_fired_message_ids().await,
        vec![newcomer],
        "exactly one notification, for the one genuinely new message"
    );
    server.stop().await;
}

#[tokio::test]
async fn smart_folder_crud_round_trips_and_reports_its_error_paths() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let created = client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
            predicate: "from:stripe is:unread".to_owned(),
            auto_tag: String::new(),
            notify: true,
        })
        .await
        .expect("create")
        .into_inner();
    assert_eq!(created.predicate, "from:stripe is:unread");
    assert!(created.auto_tag.is_empty());
    assert!(created.notify);

    let listed = client
        .list_smart_folders(ListSmartFoldersRequest {
            account_id: server.account_id,
        })
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.folders.len(), 1);

    // Free text in a predicate would silently widen membership; rejected.
    let status = client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: server.account_id,
            name: "Loose".to_owned(),
            predicate: "from:stripe invoice".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("free text");
    assert_eq!(status.code(), Code::InvalidArgument);

    // Duplicate name.
    let status = client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: server.account_id,
            name: "stripe".to_owned(),
            predicate: "from:aws".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("duplicate");
    assert_eq!(status.code(), Code::AlreadyExists);

    // An account that does not exist.
    let status = client
        .create_smart_folder(CreateSmartFolderRequest {
            account_id: 9_999,
            name: "Nowhere".to_owned(),
            predicate: "from:stripe".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("missing account");
    assert_eq!(status.code(), Code::NotFound);

    // A folder that does not exist.
    let status = client
        .evaluate_smart_folder(EvaluateSmartFolderRequest {
            account_id: server.account_id,
            name: "nope".to_owned(),
        })
        .await
        .expect_err("missing folder");
    assert_eq!(status.code(), Code::NotFound);

    client
        .delete_smart_folder(DeleteSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
        })
        .await
        .expect("delete");
    let status = client
        .delete_smart_folder(DeleteSmartFolderRequest {
            account_id: server.account_id,
            name: "Stripe".to_owned(),
        })
        .await
        .expect_err("second delete");
    assert_eq!(status.code(), Code::NotFound);
    server.stop().await;
}
