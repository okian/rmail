//! Integration test: drive `FinderService` end-to-end against an in-process
//! tonic server over a Unix domain socket, booted through the real
//! [`rmaild::serve_uds_with_engine_and_mail_store`] so the handler runs
//! beside the same drain loop, the same store, and the same `MailStore` the
//! daemon serves everything else from.
//!
//! Four things can only be proven here rather than in `rmail-core`:
//!
//! - **Batches actually stream, descending, and the last one is complete.**
//!   The core's `scan` proves the shape; this proves it survives the
//!   channel, the spawned task, and the wire.
//! - **An abandoned stream ends cleanly.** A picker that has moved on must
//!   never be handed a `CANCELLED` status, and must never be left holding a
//!   stream that will not close.
//! - **`BatchAction` goes through the real `MailStore`.** An archive from a
//!   picker moves the message in `messages`, exactly as `MailService.Move`
//!   would, and a vanished id comes back in `not_found` instead of failing
//!   the batch.
//! - **The drain loop is wired into the daemon**, so new mail becomes
//!   findable without anyone calling `RebuildIndex`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::{repo, Error};
use rmail_core::{Config, Database};
use rmail_proto::v1::finder_service_client::FinderServiceClient;
use rmail_proto::v1::{
    BatchActionRequest, FindBatch, FindRequest, FinderRebuildRequest, FinderScope,
    FinderStatusRequest, ItemKind,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous: a liveness bound on a spawned scan, not a latency measurement.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// An `ImapMutator` that records nothing and refuses nothing.
///
/// `BatchAction` runs through the real `MailStore`, which means every action
/// reaches IMAP before it touches the database — that is the property being
/// tested. `rmaild/tests/mail_service.rs` establishes this pattern for
/// exactly the same reason: what matters here is that the *local* state ends
/// up right, and the wire bytes of a `UID STORE` are already covered against
/// a real mock server in `imap::mutate`'s own tests.
#[derive(Debug, Default)]
struct FakeImap;

#[async_trait::async_trait]
impl ImapMutator for FakeImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Ok(())
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Ok(())
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Ok(())
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Ok(())
    }
    async fn store_keyword(
        &self,
        _: i64,
        _: &str,
        _: i64,
        _: &[i64],
        _: &str,
        _: bool,
        _: bool,
    ) -> Result<(), Error> {
        Ok(())
    }
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    archive_id: i64,
    next_uid: Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let mut config = Config::default();
        config.index.semantic.enabled = false;
        // Short enough that `the_drain_loop_picks_up_new_mail_without_a_rebuild`
        // does not wait a quarter second per attempt; every other test calls
        // `RebuildIndex` explicitly so nothing else depends on a timer.
        config.finder.refresh_interval_ms = 50;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-finder-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-finder-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);
        let db = Database::open(&db_path).unwrap();

        let (account_id, mailbox_id, archive_id) = db
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
                let archive_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        // Matches `rules.archive_mailbox`'s default, which is
                        // what `BatchAction`'s `archive` resolves against.
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id, archive_id))
            })
            .unwrap();

        let log = EventLog::new(db.clone(), Retention::unlimited());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let mail_store =
            MailStore::new(db.clone(), log, Arc::new(FakeImap) as Arc<dyn ImapMutator>);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
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
            account_id,
            mailbox_id,
            archive_id,
            next_uid: Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> FinderServiceClient<Channel> {
        FinderServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    fn seed(&self, subject: &str, from: &str) -> i64 {
        self.seed_many(&[(subject, from)])[0]
    }

    /// Insert several messages in one write transaction.
    ///
    /// One `with_write` per message would be one transaction — and one
    /// `finder_dirty` trigger commit — per message, which turns a few
    /// thousand rows of fixture into a minute of fsyncs.
    fn seed_many(&self, messages: &[(&str, &str)]) -> Vec<i64> {
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        let first_uid = self.next_uid.get();
        self.next_uid
            .set(first_uid + i64::try_from(messages.len()).unwrap());
        let rows: Vec<(String, String)> = messages
            .iter()
            .map(|(subject, from)| ((*subject).to_owned(), (*from).to_owned()))
            .collect();
        self.db
            .with_write(move |c| {
                let tx = c.transaction()?;
                let mut ids = Vec::with_capacity(rows.len());
                for (offset, (subject, from)) in rows.into_iter().enumerate() {
                    let uid = first_uid + i64::try_from(offset).unwrap();
                    ids.push(repo::insert_message(
                        &tx,
                        &repo::NewMessage {
                            account_id,
                            mailbox_id,
                            uid,
                            uidvalidity: 1,
                            from_addr: Some(from),
                            from_name: Some("Dana Whitfield".to_owned()),
                            subject: Some(subject),
                            date: Some(1_700_000_000 + uid),
                            ..Default::default()
                        },
                    )?);
                }
                tx.commit()?;
                Ok(ids)
            })
            .unwrap()
    }

    /// Bring the in-memory index up to date. Explicit rather than waiting on
    /// the 50 ms drain, so nothing here is timing-dependent.
    async fn rebuild(&self) {
        self.client()
            .await
            .rebuild_index(FinderRebuildRequest {})
            .await
            .unwrap();
    }

    async fn find(&self, request: FindRequest) -> Vec<FindBatch> {
        let mut stream = self
            .client()
            .await
            .find(request)
            .await
            .unwrap()
            .into_inner();
        let mut batches = Vec::new();
        while let Ok(Some(item)) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .expect("the Find stream stalled")
            .transpose()
        {
            batches.push(item);
        }
        batches
    }

    /// Whether the local `messages` row still exists.
    ///
    /// A move removes it rather than repointing `mailbox_id`: the message's
    /// UID in the destination folder is the server's to assign, so
    /// `MailStore::move_message` deletes locally and lets the destination's
    /// next sync reclaim it (see that method's own docs). "The row is gone"
    /// is therefore what a successful archive looks like from here.
    async fn message_exists(&self, message_id: i64) -> bool {
        self.db
            .read(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap()
            > 0
    }

    /// The `to_mailbox_id` of every `MOVED` event recorded for `message_id`.
    async fn moved_destinations(&self, message_id: i64) -> Vec<i64> {
        self.db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT payload FROM events WHERE kind = 'MOVED' AND message_id = ?1 \
                     ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
            .into_iter()
            .filter_map(|payload| {
                serde_json::from_str::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("to_mailbox_id")
                            .and_then(serde_json::Value::as_i64)
                    })
            })
            .collect()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

fn request(query: &str) -> FindRequest {
    FindRequest {
        query: query.to_owned(),
        scope: FinderScope::Unspecified as i32,
        account_id: 0,
        mailbox_id: 0,
        limit: 0,
        with_positions: true,
    }
}

/// A request scoped to messages.
///
/// Used wherever a test asserts an exact result *count*: under the default
/// `all` scope the command palette is genuinely in the corpus, and short
/// queries hit it in ways that are correct but not what the assertion is
/// about (`acme` is a subsequence of `archive message.archive`).
/// A batch action over message ids.
///
/// `kind` is required by the daemon and deliberately not defaulted — see
/// `BatchActionRequest.kind` on why an unstated kind is a wrong-object
/// mutation waiting to happen — so every call site goes through this.
fn action(verb: &str, ref_ids: Vec<i64>) -> BatchActionRequest {
    BatchActionRequest {
        action: verb.to_owned(),
        ref_ids,
        kind: ItemKind::Message as i32,
    }
}

fn messages(query: &str) -> FindRequest {
    FindRequest {
        scope: FinderScope::Messages as i32,
        ..request(query)
    }
}

// ---------------------------------------------------------------------------
// Find
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_streams_a_complete_descending_batch() {
    let server = TestServer::start().await;
    server.seed("Acme invoice 338", "billing@acme.com");
    server.seed("Lunch on Thursday", "sam@example.com");
    server.seed("Acme contract renewal", "legal@acme.com");
    server.rebuild().await;

    let batches = server.find(messages("acme")).await;
    assert!(!batches.is_empty(), "the stream produced nothing");
    let last = batches.last().unwrap();
    assert!(last.complete, "the last batch must be flagged complete");
    assert!(!last.superseded);
    assert!(last.scanned > 0);

    let subjects: Vec<&str> = last
        .results
        .iter()
        .map(|r| r.primary_text.as_str())
        .collect();
    assert_eq!(subjects.len(), 2, "got {subjects:?}");
    assert!(subjects.iter().all(|s| s.starts_with("Acme")));
    assert!(
        last.results.windows(2).all(|w| w[0].score >= w[1].score),
        "a batch must be descending"
    );
    // Exactly one batch carries `complete`.
    assert_eq!(batches.iter().filter(|b| b.complete).count(), 1);

    server.stop().await;
}

/// Highlight positions have to survive the wire as **char** offsets into
/// `primary_text` — the bug class `search_cli`'s own `café` test pins for
/// snippets.
#[tokio::test]
async fn find_returns_char_offset_positions() {
    let server = TestServer::start().await;
    server.seed("Café résumé draft", "dana@example.com");
    server.rebuild().await;

    let batches = server.find(messages("crd")).await;
    let last = batches.last().unwrap();
    assert_eq!(last.results.len(), 1);
    let hit = &last.results[0];
    let chars: Vec<char> = hit.primary_text.chars().collect();
    assert!(!hit.positions.is_empty(), "highlights were not computed");
    for position in &hit.positions {
        assert!(
            (*position as usize) < chars.len(),
            "position {position} is past the {} chars of {:?}",
            chars.len(),
            hit.primary_text
        );
    }
    let highlighted: String = hit
        .positions
        .iter()
        .map(|p| chars[*p as usize])
        .collect::<String>();
    assert_eq!(highlighted, "Crd");

    server.stop().await;
}

#[tokio::test]
async fn positions_are_omitted_unless_asked_for() {
    let server = TestServer::start().await;
    server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    let batches = server
        .find(FindRequest {
            with_positions: false,
            ..messages("acme")
        })
        .await;
    let last = batches.last().unwrap();
    assert_eq!(last.results.len(), 1);
    assert!(last.results[0].positions.is_empty());

    server.stop().await;
}

/// prd.md's sigil grammar, parsed server-side so no client can drift from it.
#[tokio::test]
async fn a_sigil_switches_scope_over_the_wire() {
    let server = TestServer::start().await;
    server.seed("archive everything", "sam@example.com");
    server.rebuild().await;

    // `>` selects commands, so the *message* whose subject contains
    // "archive" must not come back.
    let batches = server.find(request(">archive")).await;
    let last = batches.last().unwrap();
    assert!(!last.results.is_empty(), "the command palette is empty");
    assert!(
        last.results
            .iter()
            .all(|r| r.kind == ItemKind::Command as i32),
        "a `>` query returned a non-command"
    );
    // ...and the commands are the keymap's action ids.
    assert!(
        last.results
            .iter()
            .any(|r| r.secondary == "message.archive"),
        "the palette does not expose the keymap action ids"
    );

    server.stop().await;
}

#[tokio::test]
async fn an_explicit_scope_restricts_the_kinds() {
    let server = TestServer::start().await;
    server.seed("inbox triage", "sam@example.com");
    server.rebuild().await;

    let batches = server
        .find(FindRequest {
            scope: FinderScope::Mailboxes as i32,
            ..request("inbox")
        })
        .await;
    let last = batches.last().unwrap();
    assert_eq!(last.results.len(), 1);
    assert_eq!(last.results[0].kind, ItemKind::Mailbox as i32);
    assert_eq!(last.results[0].primary_text, "INBOX");

    server.stop().await;
}

/// prd.md's `in-folder` scope.
#[tokio::test]
async fn a_mailbox_filter_restricts_to_that_folder() {
    let server = TestServer::start().await;
    let inbox_message = server.seed("release notes", "sam@example.com");
    server.rebuild().await;

    let batches = server
        .find(FindRequest {
            mailbox_id: server.archive_id,
            ..request("release")
        })
        .await;
    assert!(
        batches.last().unwrap().results.is_empty(),
        "the message is in INBOX, not Archive"
    );

    let batches = server
        .find(FindRequest {
            mailbox_id: server.mailbox_id,
            ..request("release")
        })
        .await;
    let last = batches.last().unwrap();
    assert_eq!(last.results.len(), 1);
    assert_eq!(last.results[0].ref_id, inbox_message);

    server.stop().await;
}

/// prd.md: an empty query is the signal-ranked list a picker opens with, not
/// an error and not an empty result.
#[tokio::test]
async fn an_empty_query_returns_the_signal_ranked_list() {
    let server = TestServer::start().await;
    server.seed("Acme invoice", "billing@acme.com");
    server.rebuild().await;

    let batches = server.find(request("")).await;
    let last = batches.last().unwrap();
    assert!(
        last.results.len() > 1,
        "an empty query should list messages and commands: got {}",
        last.results.len()
    );

    server.stop().await;
}

/// The property task 85's overlay depends on when it issues one `Find` per
/// character: an abandoned stream ends *cleanly* and terminates, so a picker
/// that has moved on never has a `CANCELLED` status to render and never leaks
/// a stream that will not close.
///
/// Deliberately does **not** assert `superseded == true`. Whether the older
/// scan is still running when the newer one arrives is a genuine race — an
/// in-memory scan over a test-sized index finishes in well under the round
/// trip that supersedes it — and a test that demanded the flag would be
/// asserting scheduling luck. The flag's own behavior is pinned where it is
/// deterministic: `finder_service`'s `a_fresh_generation_cancels_the_previous_one`
/// for the slot, and `rmail_core::finder`'s `a_cancelled_scan_stops_early`
/// for the scan actually stopping.
#[tokio::test]
async fn a_superseded_find_ends_cleanly_instead_of_erroring() {
    let server = TestServer::start().await;
    let rows: Vec<(String, String)> = (0..4_000)
        .map(|uid| (format!("acme item {uid}"), "billing@acme.com".to_owned()))
        .collect();
    let borrowed: Vec<(&str, &str)> = rows
        .iter()
        .map(|(subject, from)| (subject.as_str(), from.as_str()))
        .collect();
    server.seed_many(&borrowed);
    server.rebuild().await;

    let mut first_client = server.client().await;
    let mut first = first_client
        .find(messages("acme"))
        .await
        .unwrap()
        .into_inner();

    // Supersede it before reading anything from the first stream.
    let second = server.find(messages("acme")).await;
    assert!(second.last().unwrap().complete);

    let mut batches = Vec::new();
    loop {
        let next = tokio::time::timeout(STREAM_TIMEOUT, first.next())
            .await
            .expect("the abandoned stream never terminated");
        match next {
            Some(item) => batches.push(item.expect("an abandoned stream must not error")),
            None => break,
        }
    }
    let last = batches.last().expect("the abandoned stream still ends");
    assert!(
        last.complete,
        "an abandoned stream must terminate with a complete batch"
    );
    if last.superseded {
        assert!(
            last.results.len() <= second.last().unwrap().results.len(),
            "a cut-short scan cannot have found more than a complete one"
        );
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// BatchAction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_action_archives_through_the_real_mail_store() {
    let server = TestServer::start().await;
    let first = server.seed("Acme invoice 338", "billing@acme.com");
    let second = server.seed("Acme contract", "legal@acme.com");
    server.rebuild().await;

    let response = server
        .client()
        .await
        .batch_action(action("archive", vec![first, second]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.applied, 2);
    assert!(response.not_found.is_empty());
    // Same observable outcome `MailService.Move` produces, because it is the
    // same `MailStore::move_message` call: the local row is released to the
    // destination's next sync, and a `MOVED` event names where it went.
    assert!(!server.message_exists(first).await);
    assert!(!server.message_exists(second).await);
    assert_eq!(
        server.moved_destinations(first).await,
        vec![server.archive_id]
    );
    assert_eq!(
        server.moved_destinations(second).await,
        vec![server.archive_id]
    );

    server.stop().await;
}

/// `rules.archive_mailbox` names a folder this account may not have, and that
/// is a precondition failure — not a stale ref, and not a silent no-op that
/// reports success.
#[tokio::test]
async fn archiving_without_an_archive_folder_is_a_precondition_failure() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;
    let archive_id = server.archive_id;
    server
        .db
        .with_write(move |conn| {
            conn.execute("DELETE FROM mailboxes WHERE id = ?1", [archive_id])?;
            Ok(())
        })
        .unwrap();

    let status = server
        .client()
        .await
        .batch_action(action("archive", vec![message]))
        .await
        .expect_err("there is nowhere to archive to");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("Archive"), "{}", status.message());
    assert!(server.message_exists(message).await, "nothing was moved");

    server.stop().await;
}

#[tokio::test]
async fn batch_action_sets_and_clears_flags_without_losing_the_others() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;
    let mut client = server.client().await;

    client
        .batch_action(action("flag", vec![message]))
        .await
        .unwrap();
    client
        .batch_action(action("read", vec![message]))
        .await
        .unwrap();

    let flags: Vec<String> = server
        .db
        .read(move |conn| repo::list_flags(conn, message))
        .await
        .unwrap();
    assert!(
        flags.iter().any(|f| f == "\\Flagged"),
        "marking read cleared the flag: {flags:?}"
    );
    assert!(flags.iter().any(|f| f == "\\Seen"), "got {flags:?}");

    client
        .batch_action(action("unread", vec![message]))
        .await
        .unwrap();
    let flags: Vec<String> = server
        .db
        .read(move |conn| repo::list_flags(conn, message))
        .await
        .unwrap();
    assert!(!flags.iter().any(|f| f == "\\Seen"), "got {flags:?}");
    assert!(flags.iter().any(|f| f == "\\Flagged"), "got {flags:?}");

    server.stop().await;
}

/// prd.md: "Stale ref -> action returns not_found." One vanished id must not
/// cost the other nineteen.
#[tokio::test]
async fn batch_action_reports_a_stale_ref_without_failing_the_batch() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    let response = server
        .client()
        .await
        .batch_action(action("read", vec![message, 999_999]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.applied, 1);
    assert_eq!(response.not_found, vec![999_999]);

    server.stop().await;
}

/// The wrong-object guard, over the wire. A tag id and a message id are the
/// same integer space, so a client that forwards an unfiltered selection must
/// be refused rather than quietly acted upon.
#[tokio::test]
async fn a_batch_action_over_non_message_ids_is_rejected() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    for wrong in [ItemKind::Unspecified, ItemKind::Tag, ItemKind::Mailbox] {
        let status = server
            .client()
            .await
            .batch_action(BatchActionRequest {
                action: "delete".to_owned(),
                ref_ids: vec![message],
                kind: wrong as i32,
            })
            .await
            .expect_err("only message ids may be acted on");
        assert_eq!(status.code(), Code::InvalidArgument, "{wrong:?}");
    }
    // ...and the message is untouched by any of it.
    assert!(server.message_exists(message).await);

    server.stop().await;
}

/// Every id is an IMAP round trip, so an uncapped batch would let a client
/// decide how long graceful shutdown is held open.
#[tokio::test]
async fn an_oversized_batch_is_rejected_before_anything_is_applied() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    let mut ref_ids = vec![message];
    ref_ids.extend(1_000_000..1_002_000);
    let status = server
        .client()
        .await
        .batch_action(action("delete", ref_ids))
        .await
        .expect_err("an unbounded batch must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        server.message_exists(message).await,
        "the batch was refused, so nothing may have been applied"
    );

    server.stop().await;
}

#[tokio::test]
async fn an_unknown_action_is_rejected() {
    let server = TestServer::start().await;
    let message = server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    let status = server
        .client()
        .await
        .batch_action(action("launch_missiles", vec![message]))
        .await
        .expect_err("an unknown action must not be a silent no-op");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("launch_missiles"));

    server.stop().await;
}

// ---------------------------------------------------------------------------
// index maintenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_status_reports_a_populated_index() {
    let server = TestServer::start().await;
    server.seed("Acme invoice 338", "billing@acme.com");
    server.rebuild().await;

    let status = server
        .client()
        .await
        .index_status(FinderStatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(status.entries > 0);
    assert!(status.bytes > 0);
    assert_eq!(status.rejected, 0);
    assert!(status.refreshed_at > 0);

    server.stop().await;
}

/// The background drain is what keeps the index live without a rebuild, and
/// it is wired into the daemon rather than only into the core's tests.
#[tokio::test]
async fn the_drain_loop_picks_up_new_mail_without_a_rebuild() {
    let server = TestServer::start().await;
    server.rebuild().await;
    server.seed("Zephyrine quarterly", "dana@example.com");

    let mut found = false;
    // `refresh_interval_ms` is 50 for this server; twenty ticks of headroom.
    for _ in 0..100 {
        let batches = server.find(request("zephyrine")).await;
        if !batches.last().unwrap().results.is_empty() {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(found, "the drain loop never picked up the new message");

    server.stop().await;
}
