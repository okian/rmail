//! Transport tests: drive [`GrpcExec`] against a **real in-process `rmaild`**
//! over a real Unix socket, and assert the [`Msg`]s that come back.
//!
//! Not a mock of the transport (`CLAUDE.md`: "Integration tests run against an
//! in-process `tonic` server, not mocks of the transport"). The point is
//! exactly the layer the model tests cannot reach: that each [`Cmd`] names the
//! RPC it claims to, that the wire types map onto the model's, and that a
//! `tonic::Status` becomes a string a status line can show rather than being
//! swallowed.
//!
//! These live inside the binary crate rather than in `rmail-cli/tests/`
//! because `rmail-cli` has no lib target — `search_cli`'s own unit tests give
//! the same reason — so `tests/` can only exec the built `mail` binary, which
//! cannot reach [`GrpcExec`] at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::{Config, Database, Error};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::*;
use crate::tui::model::{Folder, MessageRow, OpenMessage};
use crate::tui::report::ReportFill;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a test waits for a background task to answer. Generous: the gate
/// runs several test binaries at once in a memory-constrained container.
const DEADLINE: Duration = Duration::from_secs(30);

/// A quoted-printable, RFC 2047-encoded message — the same shape
/// `wire::tests` uses, so what lands here has genuinely crossed the wire
/// after `parse_message` decoded it daemon-side.
const RAW: &[u8] = b"From: =?UTF-8?Q?Zo=C3=AB?= <zoe@example.com>\r\n\
To: me@example.com\r\n\
Subject: =?UTF-8?B?SW52b2ljZSDigqwxMA==?=\r\n\
Content-Type: multipart/alternative; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Total: =E2=82=AC10 =3D cheap\r\n\
--b\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>Total: &euro;10</p>\r\n\
--b--\r\n";

/// An `ImapMutator` that accepts everything without a network.
///
/// The mutating RPCs (`SetFlags`/`Move`/`Copy`/`Delete`) reflect to IMAP
/// *before* touching the local mirror, so against a default daemon every one
/// of them fails `FAILED_PRECONDITION: account has no IMAP server configured`
/// and the local half of the contract — the half the TUI actually reacts to —
/// is never reached. `rmaild/tests/mail_service.rs` stands up the same fake
/// for the same reason; `rmail_core::imap::mutate`'s own tests already prove
/// the real wire commands.
#[derive(Debug, Default)]
struct AcceptingImap;

#[async_trait::async_trait]
impl ImapMutator for AcceptingImap {
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

    /// Tagging's keyword push. Nothing in this suite tags anything, but the
    /// trait is what `MailStore` takes, so it has to be complete.
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

struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    account_id: i64,
    inbox_id: i64,
    archive_id: i64,
    message_id: i64,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), rmaild::ServeError>>,
}

impl Daemon {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        // A short path: a Unix socket's `sun_path` is ~104 bytes, and the
        // per-test temp dir this workspace's helpers build elsewhere is long
        // enough to matter.
        let socket = PathBuf::from("/tmp").join(format!("rmail-tui-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-tui-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
        }
        let db = Database::open(&db_path).expect("open db");

        let (account_id, inbox_id, archive_id, message_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &NewAccount {
                        name: format!("personal-{n}"),
                        username: Some("me@example.com".to_owned()),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                let archive_id = repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )?;
                let parsed = rmail_core::message::parse::parse_message(RAW);
                let message_id = repo::insert_message(
                    c,
                    &NewMessage {
                        account_id,
                        mailbox_id: inbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        subject: parsed.subject.clone(),
                        from_addr: parsed.from_addr.clone(),
                        from_name: parsed.from_name.clone(),
                        date: Some(1_700_000_000),
                        body_text: parsed.body_text.clone(),
                        body_html: parsed.body_html.clone(),
                        raw: Some(RAW.to_vec()),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id, archive_id, message_id))
            })
            .expect("seed");

        let log = EventLog::new(db.clone(), Retention::default());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let mail_store = MailStore::new(
            db.clone(),
            log,
            Arc::new(AcceptingImap) as Arc<dyn ImapMutator>,
        );

        let (shutdown, rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            let mut config = Config::default();
            config.index.semantic.enabled = false;
            rmaild::serve_uds_with_engine_and_mail_store(
                &server_socket,
                db,
                engine,
                mail_store,
                &config,
                async move {
                    let _ = rx.await;
                },
            )
            .await
        });

        for _ in 0..500 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            socket,
            db_path,
            account_id,
            inbox_id,
            archive_id,
            message_id,
            shutdown,
            handle,
        }
    }

    async fn exec(&self) -> GrpcExec {
        GrpcExec::connect(&self.socket)
            .await
            .expect("connect to the in-process daemon")
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(DEADLINE, self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.db_path.display()));
        }
    }
}

fn channel() -> (UnboundedSender<Msg>, UnboundedReceiver<Msg>) {
    mpsc::unbounded_channel()
}

/// The next message, or a failure naming what was being waited for.
async fn next(rx: &mut UnboundedReceiver<Msg>, what: &str) -> Msg {
    match tokio::time::timeout(DEADLINE, rx.recv()).await {
        Ok(Some(msg)) => msg,
        Ok(None) => unreachable!("the executor dropped the channel waiting for {what}"),
        Err(_) => unreachable!("timed out waiting for {what}"),
    }
}

#[tokio::test]
async fn load_accounts_folders_and_messages_come_back_as_model_types() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(Cmd::LoadAccounts, tx.clone());
    let accounts = match next(&mut rx, "accounts").await {
        Msg::Accounts(Ok(accounts)) => accounts,
        other => unreachable!("expected accounts, got {other:?}"),
    };
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, daemon.account_id);
    assert_eq!(
        accounts[0].username.as_deref(),
        Some("me@example.com"),
        "the reply's From address is carried through"
    );

    exec.exec(
        Cmd::LoadFolders {
            account_id: daemon.account_id,
        },
        tx.clone(),
    );
    // `SyncService.Status` answers two questions at once, so this command sends
    // two messages — see `a_folder_listing_also_reports_the_sync_indicator`.
    match next(&mut rx, "the sync indicator").await {
        Msg::Daemon {
            subsystem: Subsystem::Sync,
            ..
        } => {}
        other => unreachable!("expected the sync indicator first, got {other:?}"),
    }
    let folders: Vec<Folder> = match next(&mut rx, "folders").await {
        Msg::Folders(Ok(folders)) => folders,
        other => unreachable!("expected folders, got {other:?}"),
    };
    // Proves the folder pane's source of truth: `SyncService.Status` lists
    // every mailbox, including ones sync has never touched.
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"INBOX") && names.contains(&"Archive"),
        "{names:?}"
    );

    exec.exec(
        Cmd::LoadMessages {
            mailbox_id: daemon.inbox_id,
        },
        tx.clone(),
    );
    let rows: Vec<MessageRow> = match next(&mut rx, "messages").await {
        Msg::Messages {
            mailbox_id,
            result: Ok(rows),
        } => {
            assert_eq!(mailbox_id, daemon.inbox_id, "the reply names its folder");
            rows
        }
        other => unreachable!("expected messages, got {other:?}"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, daemon.message_id);
    assert_eq!(rows[0].subject, "Invoice €10", "decoded, over the wire");
    assert_eq!(rows[0].from, "Zoë");
    assert_eq!(rows[0].from_addr.as_deref(), Some("zoe@example.com"));

    daemon.stop().await;
}

#[tokio::test]
async fn open_returns_the_decoded_body_the_daemon_parsed() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Open {
            message_id: daemon.message_id,
        },
        tx,
    );
    let open: OpenMessage = match next(&mut rx, "the opened message").await {
        Msg::Opened {
            message_id,
            result: Ok(open),
        } => {
            assert_eq!(message_id, daemon.message_id, "the reply names its request");
            open
        }
        other => unreachable!("expected an opened message, got {other:?}"),
    };

    let body = open.body.join("\n");
    assert!(body.contains("€10"), "quoted-printable decoded: {body:?}");
    assert!(open.has_html, "the HTML alternative reached the client");
    assert!(open
        .headers
        .iter()
        .any(|(name, value)| name == "Subject" && value == "Invoice €10"));

    daemon.stop().await;
}

#[tokio::test]
async fn a_status_error_reaches_the_status_line_instead_of_being_swallowed() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Open {
            message_id: 999_999,
        },
        tx,
    );
    match next(&mut rx, "the failed open").await {
        Msg::Opened {
            message_id,
            result: Err(error),
        } => {
            assert_eq!(message_id, 999_999);
            assert!(
                error.contains("999999") || error.to_lowercase().contains("not found"),
                "the daemon's own words survive the trip: {error:?}"
            );
        }
        other => unreachable!("expected a NOT_FOUND, got {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn set_flags_reaches_the_daemon_and_reports_the_set_it_applied() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::SetFlags {
            message_id: daemon.message_id,
            flags: vec![crate::tui::model::SEEN.to_owned()],
            label: "marked read".to_owned(),
        },
        tx.clone(),
    );
    match next(&mut rx, "the flag update").await {
        Msg::Done {
            label,
            result: Ok(Effect::Flags { message_id, flags }),
        } => {
            assert_eq!(label, "marked read");
            assert_eq!(message_id, daemon.message_id);
            assert_eq!(flags, vec![crate::tui::model::SEEN.to_owned()]);
        }
        other => unreachable!("expected a flag effect, got {other:?}"),
    }

    // And the list now reads it back, which is what proves the write landed
    // rather than the client merely echoing what it sent.
    exec.exec(
        Cmd::LoadMessages {
            mailbox_id: daemon.inbox_id,
        },
        tx,
    );
    match next(&mut rx, "the reloaded list").await {
        Msg::Messages {
            result: Ok(rows), ..
        } => assert!(
            rows[0].has_flag(crate::tui::model::SEEN),
            "flags after the round trip: {:?}",
            rows[0].flags
        ),
        other => unreachable!("expected messages, got {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn a_reply_becomes_a_draft_composeservice_actually_stored() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Draft {
            kind: crate::tui::model::DraftKind::Reply,
            account_id: daemon.account_id,
            from: "me@example.com".to_owned(),
            to: "zoe@example.com".to_owned(),
            message_id: daemon.message_id,
        },
        tx,
    );
    let draft_id = match next(&mut rx, "the created draft").await {
        Msg::Done {
            result: Ok(Effect::Drafted(id)),
            ..
        } => id,
        other => unreachable!("expected a draft, got {other:?}"),
    };

    // Read it back through the service, not the database: the TUI's claim is
    // that reply/forward go through `ComposeService`, and this is what makes
    // that checkable.
    let channel = rmail_core::connect_uds(&daemon.socket).await.unwrap();
    let draft = rmail_proto::v1::compose_service_client::ComposeServiceClient::new(channel)
        .get_draft(rmail_proto::v1::GetDraftRequest { draft_id })
        .await
        .expect("GetDraft")
        .into_inner();

    assert_eq!(draft.subject, "Re: Invoice €10");
    assert_eq!(
        draft.to.first().map(|a| a.address.as_str()),
        Some("zoe@example.com")
    );
    assert_eq!(
        draft.in_reply_to_message_id,
        Some(daemon.message_id),
        "the reply threads onto the message it answers"
    );
    assert!(
        draft.body_text.contains("> Total: €10"),
        "the decoded original is quoted: {:?}",
        draft.body_text
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_move_removes_the_row_and_the_listing_agrees() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Move {
            message_id: daemon.message_id,
            dest_mailbox_id: daemon.archive_id,
            label: "archived".to_owned(),
        },
        tx.clone(),
    );
    match next(&mut rx, "the move").await {
        Msg::Done {
            label,
            result: Ok(Effect::Removed(id)),
        } => {
            assert_eq!(label, "archived");
            assert_eq!(id, daemon.message_id);
        }
        other => unreachable!("expected a move result, got {other:?}"),
    }

    exec.exec(
        Cmd::LoadMessages {
            mailbox_id: daemon.inbox_id,
        },
        tx,
    );
    match next(&mut rx, "the reloaded list").await {
        Msg::Messages {
            result: Ok(rows), ..
        } => assert!(rows.is_empty(), "the moved message left the source folder"),
        other => unreachable!("expected messages, got {other:?}"),
    }

    daemon.stop().await;
}

/// Task 94's verbs, through the daemon rather than through the model.
///
/// The layer a table test structurally cannot reach: that each `Cmd` names an
/// RPC this daemon actually serves, and that its answer maps onto rows. One
/// test rather than seventeen because the interesting failure is uniform — a
/// misnamed method or a response shape the mapping does not fit — and a
/// per-verb test would spend seventeen daemon startups proving the same thing.
#[tokio::test]
async fn every_daemon_report_verb_reaches_an_rpc_and_comes_back_as_rows() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    let unary = [
        ("index status", Cmd::IndexStatus { generation: 1 }),
        ("index verify", Cmd::IndexVerify { generation: 2 }),
        ("index gc", Cmd::IndexGc { generation: 3 }),
        (
            "index entities",
            Cmd::IndexEntities {
                generation: 4,
                kind: "email".to_owned(),
            },
        ),
        (
            "sync status",
            Cmd::SyncStatusReport {
                generation: 5,
                account_id: daemon.account_id,
            },
        ),
        (
            "ai status",
            Cmd::AiUsage {
                generation: 6,
                costs: false,
            },
        ),
        (
            "ai cost",
            Cmd::AiUsage {
                generation: 7,
                costs: true,
            },
        ),
        ("finder status", Cmd::FinderStatus { generation: 8 }),
    ];
    for (verb, cmd) in unary {
        let generation = match &cmd {
            Cmd::IndexStatus { generation }
            | Cmd::IndexVerify { generation }
            | Cmd::IndexGc { generation }
            | Cmd::IndexEntities { generation, .. }
            | Cmd::SyncStatusReport { generation, .. }
            | Cmd::AiUsage { generation, .. }
            | Cmd::FinderStatus { generation } => *generation,
            other => unreachable!("not a unary report: {other:?}"),
        };
        exec.exec(cmd, tx.clone());
        match next(&mut rx, verb).await {
            Msg::Report {
                generation: got,
                event:
                    ReportEvent::Frame {
                        fill: ReportFill::Replace,
                        rows,
                        complete: true,
                    },
            } => {
                assert_eq!(got, generation, "{verb} answered under another stamp");
                // `index entities` on a seeded-but-unindexed daemon is legitimately
                // empty; every other one of these reports at least one row.
                if verb != "index entities" {
                    assert!(!rows.is_empty(), "{verb} answered with no rows");
                }
            }
            other => unreachable!("{verb}: expected one complete frame, got {other:?}"),
        }
    }

    exec.shutdown();
    daemon.stop().await;
}

/// The streaming half: a rebuild reports progress and finishes.
///
/// `Rebuild` over a seeded mailbox with the semantic stage off is fast, which is
/// why this is the streaming verb under test — `Reindex` and `AnalyzeMessage`
/// share the drain loop, and `AnalyzeMessage` needs a model provider this
/// harness deliberately does not configure.
#[tokio::test]
async fn a_streamed_rebuild_reports_progress_and_completes() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(Cmd::IndexRebuild { generation: 9 }, tx.clone());

    // Frames until one says complete. Bounded, so a daemon that never finished
    // fails the test rather than hanging it.
    let mut frames = 0;
    let mut finished = false;
    for _ in 0..64 {
        match next(&mut rx, "a rebuild frame").await {
            Msg::Report {
                generation: 9,
                event: ReportEvent::Frame { rows, complete, .. },
            } => {
                frames += 1;
                assert!(!rows.is_empty(), "a progress frame carries counters");
                if complete {
                    finished = true;
                    break;
                }
            }
            Msg::Report {
                generation: 9,
                event: ReportEvent::Failed(error),
            } => unreachable!("the rebuild failed: {error}"),
            other => unreachable!("expected a rebuild frame, got {other:?}"),
        }
    }
    assert!(finished, "the rebuild never reported a terminal frame");
    assert!(frames >= 1);

    exec.shutdown();
    daemon.stop().await;
}

/// A control verb answers with a fact, and the fact reaches the status line.
#[tokio::test]
async fn the_daemon_control_verbs_answer_with_a_labelled_fact() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    for (cmd, expected) in [
        (
            Cmd::IndexSetPaused {
                pause: crate::tui::commands::Pause::Stop,
            },
            "indexer stopped",
        ),
        (
            Cmd::IndexSetPaused {
                pause: crate::tui::commands::Pause::Start,
            },
            "indexer started",
        ),
        (
            Cmd::SyncSetPaused {
                account_id: daemon.account_id,
                pause: crate::tui::commands::Pause::Stop,
            },
            "sync stopped",
        ),
        (
            Cmd::SyncSetPaused {
                account_id: daemon.account_id,
                pause: crate::tui::commands::Pause::Start,
            },
            "sync started",
        ),
    ] {
        exec.exec(cmd, tx.clone());
        match next(&mut rx, expected).await {
            Msg::Done { label, result } => {
                assert_eq!(label, expected);
                assert!(result.is_ok(), "{expected}: {result:?}");
            }
            other => unreachable!("expected a fact, got {other:?}"),
        }
    }

    // `FinderRebuild` reports its own count in the label, which is the whole
    // answer — a generic "done" would drop it.
    exec.exec(Cmd::FinderRebuild, tx.clone());
    match next(&mut rx, "the finder rebuild").await {
        Msg::Done { label, result } => {
            assert!(label.contains("entries"), "{label}");
            assert!(result.is_ok(), "{result:?}");
        }
        other => unreachable!("expected a fact, got {other:?}"),
    }

    exec.shutdown();
    daemon.stop().await;
}

/// Task 92's supersession clause, at the layer it lives in.
///
/// `SyncService.Status` is both the folder listing and the sync indicator's own
/// answer, so one call reports both — which is what lets a reload preempt the
/// heartbeat's next tick instead of the two racing to say the same thing. A
/// model test cannot see this: it is entirely about what the executor sends.
#[tokio::test]
async fn a_folder_listing_also_reports_the_sync_indicator() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::LoadFolders {
            account_id: daemon.account_id,
        },
        tx.clone(),
    );

    // The health first, so the indicator is fresh even for a reader that
    // stopped consuming folder listings.
    match next(&mut rx, "the sync indicator").await {
        Msg::Daemon {
            subsystem: Subsystem::Sync,
            result: Ok(health),
        } => {
            assert_eq!(health.state, crate::tui::status::HealthState::Ok);
            assert!(
                health.detail.contains("folder"),
                "a fresh daemon is not paused and has folders: {health:?}"
            );
        }
        other => unreachable!("expected the sync indicator, got {other:?}"),
    }
    match next(&mut rx, "the folder listing").await {
        Msg::Folders(Ok(folders)) => assert_eq!(folders.len(), 2, "{folders:?}"),
        other => unreachable!("expected the folder listing, got {other:?}"),
    }

    exec.shutdown();
    daemon.stop().await;
}

/// Task 92's heartbeat, through the daemon.
///
/// Four RPCs this daemon actually serves, four indicators — the thing a model
/// test structurally cannot prove. Ordering is asserted only as a set, because
/// the point of four messages rather than one is that a slow subsystem does not
/// hold up the others.
#[tokio::test]
async fn the_heartbeat_reports_every_subsystem_it_claims_to_ask_about() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Heartbeat {
            account_id: daemon.account_id,
        },
        tx.clone(),
    );

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..Subsystem::ALL.len() {
        match next(&mut rx, "a heartbeat answer").await {
            Msg::Daemon { subsystem, result } => {
                assert!(
                    result.is_ok(),
                    "{subsystem:?} could not be asked: {result:?}"
                );
                seen.insert(format!("{subsystem:?}"));
            }
            other => unreachable!("expected a heartbeat answer, got {other:?}"),
        }
    }
    assert_eq!(
        seen.len(),
        Subsystem::ALL.len(),
        "every subsystem answered exactly once in the first round: {seen:?}"
    );

    exec.shutdown();
    daemon.stop().await;
}

/// Task 90's report, through the daemon rather than through the model.
///
/// The one thing a model test structurally cannot prove: that `Cmd::AuthStatus`
/// names an RPC this daemon actually serves, and that both halves of the answer
/// — the daemon's two settings and this client's own credential — arrive as
/// frames the pane can apply. A mechanism whose rows only ever came from a test
/// fixture would be the "shipped inert" failure this project keeps finding.
#[tokio::test]
async fn auth_status_reports_the_daemons_gate_and_this_clients_credential() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(Cmd::AuthStatus { generation: 3 }, tx.clone());

    // The daemon's frame: a snapshot, so it replaces.
    let settings = match next(&mut rx, "the auth settings").await {
        Msg::Report {
            generation: 3,
            event:
                ReportEvent::Frame {
                    fill: ReportFill::Replace,
                    rows,
                    complete: false,
                },
        } => rows,
        other => unreachable!("expected the settings frame, got {other:?}"),
    };
    let cells: Vec<String> = settings.iter().map(|row| row.cells.join(" ")).collect();
    assert_eq!(cells.len(), 2, "{cells:?}");
    assert!(
        cells[0].starts_with("password not configured"),
        "a fresh daemon has no password gate: {cells:?}"
    );
    assert!(
        cells[1].starts_with("local login not required"),
        "and does not require a socket peer to log in: {cells:?}"
    );
    assert_eq!(
        settings[0].tone,
        ReportTone::Muted,
        "an absent gate is a default, not a fault"
    );
    assert!(
        settings[0].on_enter.is_none(),
        "there is nothing to clear, so the row offers nothing"
    );

    // This client's frame: a different source, so it appends — and it is the
    // last, so it completes the report.
    let mine = match next(&mut rx, "the credential row").await {
        Msg::Report {
            generation: 3,
            event:
                ReportEvent::Frame {
                    fill: ReportFill::Append,
                    rows,
                    complete: true,
                },
        } => rows,
        other => unreachable!("expected the credential frame, got {other:?}"),
    };
    assert_eq!(mine.len(), 1, "{mine:?}");
    let row = mine
        .first()
        .map(|row| row.cells.join(" "))
        .unwrap_or_default();
    assert!(
        row.starts_with("this client presents"),
        "and it names the kind, never the secret: {row}"
    );

    exec.shutdown();
    daemon.stop().await;
}

#[tokio::test]
async fn shutdown_stops_the_event_stream_rather_than_leaving_it_running() {
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    exec.exec(
        Cmd::Watch {
            account_id: daemon.account_id,
        },
        tx.clone(),
    );
    // Let the subscription establish, then tear it down. `WatchEvents` never
    // completes on its own, so if cancellation did not reach it the task would
    // outlive the session.
    tokio::time::sleep(Duration::from_millis(200)).await;
    exec.shutdown();
    drop(tx);

    // The only sender left is the stream task's clone; the channel closing is
    // therefore proof that task is gone.
    let closed = tokio::time::timeout(DEADLINE, async { while rx.recv().await.is_some() {} }).await;
    assert!(closed.is_ok(), "the WatchEvents task outlived shutdown");

    daemon.stop().await;
}

#[tokio::test]
async fn cmd_write_keybinding_runs_through_the_real_executor_and_reports_back() {
    // `Cmd::WriteKeybinding` reaches no RPC at all — every `model::tests`
    // covering it hand-calls `write_keybinding` and hand-builds
    // `Msg::KeysWritten`, which proves the dispatch and the message
    // handling but nothing about the thing task 102's P1 fix actually
    // delivered: that the write really runs through `spawn_blocking` behind
    // the real executor and a real `Msg` comes back on the channel, rather
    // than a swapped label or a dropped result compiling clean and never
    // being caught. The daemon this harness starts is unused by the
    // command itself; it is the only way this file constructs a
    // [`GrpcExec`] at all.
    let daemon = Daemon::start().await;
    let exec = daemon.exec().await;
    let (tx, mut rx) = channel();

    let path = std::env::temp_dir().join(format!(
        "rmail-grpc-tests-write-keybinding-{}-{}.toml",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);

    exec.exec(
        Cmd::WriteKeybinding {
            path: path.clone(),
            mode: rmail_core::keymap::Mode::Normal,
            chord: rmail_core::keymap::Chord::parse("z").expect("z parses"),
            action: rmail_core::keymap::Action::CursorDown,
            label: "bound z to cursor.down in normal mode".to_owned(),
        },
        tx.clone(),
    );
    match next(&mut rx, "keys written").await {
        Msg::KeysWritten { label, result } => {
            assert!(result.is_ok(), "{result:?}");
            assert!(label.contains("cursor.down"), "{label}");
        }
        other => unreachable!("expected KeysWritten, got {other:?}"),
    }

    let written = std::fs::read_to_string(&path).expect("the write actually landed on disk");
    assert!(written.contains("cursor.down"), "{written}");
    let _ = std::fs::remove_file(&path);

    daemon.stop().await;
}
