//! Integration test: drive the compiled `mail export` subcommand end-to-end
//! against an in-process daemon, reached exactly the way a real user would —
//! over the Unix socket, through the built `mail` binary.
//!
//! `rmail-core::export`'s tests already prove the framing and the round trips
//! with no process boundary involved. What only means something once the
//! binary is parsing flags and writing real files is here: `-o` producing a
//! file whose bytes are the stored RFC822, a directory format producing a
//! real Maildir tree, `-` streaming to stdout, and a refused request leaving
//! nothing behind on disk.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::{repo, Database};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    dir: PathBuf,
    db: Database,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-exp-cli-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-exp-cli-{pid}-{n}.db"));
        let dir = std::env::temp_dir().join(format!("rmail-exp-cli-{pid}-{n}.out"));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(&db_path).unwrap();

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
        assert!(ready, "daemon never became ready");

        Self {
            socket,
            db_path,
            dir,
            db,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn seed(&self, count: i64) {
        self.db
            .write(move |c| {
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
                for uid in 1..=count {
                    repo::insert_message(
                        c,
                        &repo::NewMessage {
                            account_id,
                            mailbox_id,
                            uid,
                            uidvalidity: 1,
                            subject: Some(format!("Report {uid}")),
                            from_addr: Some("ada@example.com".to_owned()),
                            body_text: Some("body".to_owned()),
                            raw: Some(raw_message(uid)),
                            date: Some(1_700_000_000 + uid),
                            internaldate: Some(1_700_000_000 + uid),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    /// A message row whose raw RFC822 was never stored.
    async fn seed_without_raw(&self, uid: i64) {
        self.db
            .write(move |c| {
                let (account_id, mailbox_id): (i64, i64) = c.query_row(
                    "SELECT a.id, m.id FROM accounts a JOIN mailboxes m ON m.account_id = a.id \
                     LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("no raw".to_owned()),
                        from_addr: Some("ada@example.com".to_owned()),
                        date: Some(1_800_000_000 + uid),
                        internaldate: Some(1_800_000_000 + uid),
                        ..Default::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_mail"))
            .args(args)
            .env(rmail_core::SOCKET_ENV, &self.socket)
            .output()
            .await
            .unwrap_or_else(|e| panic!("running `mail {}`: {e}", args.join(" ")))
    }

    async fn ok(&self, args: &[&str]) -> std::process::Output {
        let output = self.run(args).await;
        assert!(
            output.status.success(),
            "`mail {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    async fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args).await;
        assert!(
            !output.status.success(),
            "`mail {}` should have failed",
            args.join(" ")
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn out(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(30), self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.dir);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

fn raw_message(n: i64) -> Vec<u8> {
    format!(
        "Message-ID: <msg-{n}@example.com>\r\n\
         From: Ada <ada@example.com>\r\n\
         Subject: Report {n}\r\n\
         \r\n\
         Body of report {n}.\r\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn export_writes_an_mbox_file_containing_the_stored_bytes() {
    let server = TestServer::start().await;
    server.seed(3).await;
    let out = server.out("archive.mbox");

    let output = server
        .ok(&[
            "export",
            "",
            "--format",
            "mbox",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exported 3 message(s)"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let archive = std::fs::read(&out).unwrap();
    assert!(archive.starts_with(b"From ada@example.com "));
    for uid in 1..=3 {
        let raw = raw_message(uid);
        assert!(
            archive.windows(raw.len()).any(|w| w == raw.as_slice()),
            "message {uid} is not in {} verbatim",
            out.display()
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn export_writes_a_real_maildir_tree() {
    let server = TestServer::start().await;
    server.seed(2).await;
    let out = server.out("archive.maildir");

    server
        .ok(&[
            "export",
            "",
            "--format",
            "maildir",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;

    for sub in ["tmp", "new", "cur"] {
        assert!(out.join(sub).is_dir(), "missing maildir subdirectory {sub}");
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(out.join("cur"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2);
    assert_eq!(std::fs::read(&files[0]).unwrap(), raw_message(1));
    assert_eq!(std::fs::read(&files[1]).unwrap(), raw_message(2));
    server.stop().await;
}

#[tokio::test]
async fn export_writes_one_eml_per_message() {
    let server = TestServer::start().await;
    server.seed(2).await;
    let out = server.out("eml");

    server
        .ok(&["export", "", "--format", "eml", "-o", out.to_str().unwrap()])
        .await;

    let mut files: Vec<PathBuf> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2);
    for file in &files {
        assert!(file.extension().is_some_and(|e| e == "eml"), "{file:?}");
    }
    server.stop().await;
}

#[tokio::test]
async fn export_json_to_stdout_is_a_single_valid_document() {
    let server = TestServer::start().await;
    server.seed(2).await;

    let output = server
        .ok(&["export", "", "--format", "json", "-o", "-"])
        .await;
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(document["messages"].as_array().unwrap().len(), 2);
    // Parsing stdout above already proves the archive is not contaminated;
    // this pins the other half of the rule — with `-o -` the summary is not
    // printed at all, rather than printed somewhere else. Checked by content
    // rather than by an empty stderr, because linked C libraries (onnxruntime
    // on an unrecognized CPU) write their own warnings there.
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("exported"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_query_narrows_the_archive() {
    let server = TestServer::start().await;
    server.seed(3).await;
    let out = server.out("none.mbox");

    let output = server
        .ok(&[
            "export",
            "from:nobody@example.com",
            "--format",
            "mbox",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("exported 0 message(s)"));
    assert_eq!(std::fs::read(&out).unwrap(), Vec::<u8>::new());
    server.stop().await;
}

#[tokio::test]
async fn a_refused_export_leaves_no_file_behind() {
    let server = TestServer::start().await;
    server.seed(1).await;
    let out = server.out("never-created.mbox");

    let stderr = server
        .fails(&[
            "export",
            "--thread",
            "4242",
            "--format",
            "mbox",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert!(stderr.contains("thread 4242"), "stderr: {stderr}");
    assert!(
        !out.exists(),
        "a refused export must not leave a stub archive at {}",
        out.display()
    );
    server.stop().await;
}

#[tokio::test]
async fn with_ai_on_a_byte_format_is_refused_and_writes_nothing() {
    let server = TestServer::start().await;
    server.seed(1).await;
    let out = server.out("with-ai.mbox");

    let stderr = server
        .fails(&[
            "export",
            "",
            "--format",
            "mbox",
            "--with-ai",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert!(stderr.contains("with-ai"), "stderr: {stderr}");
    assert!(!out.exists());
    server.stop().await;
}

#[tokio::test]
async fn a_directory_format_cannot_be_streamed_to_stdout() {
    let server = TestServer::start().await;
    let stderr = server
        .fails(&["export", "", "--format", "maildir", "-o", "-"])
        .await;
    assert!(stderr.contains("directory"), "stderr: {stderr}");
    server.stop().await;
}

#[tokio::test]
async fn an_existing_archive_is_not_replaced_without_force() {
    let server = TestServer::start().await;
    server.seed(2).await;
    let out = server.out("archive.mbox");
    std::fs::write(&out, b"yesterday's archive").unwrap();

    let stderr = server
        .fails(&["export", "", "-o", out.to_str().unwrap()])
        .await;
    assert!(stderr.contains("--force"), "stderr: {stderr}");
    assert_eq!(std::fs::read(&out).unwrap(), b"yesterday's archive");

    server
        .ok(&["export", "", "-o", out.to_str().unwrap(), "--force"])
        .await;
    assert_ne!(std::fs::read(&out).unwrap(), b"yesterday's archive");
    server.stop().await;
}

#[tokio::test]
async fn a_message_with_no_stored_raw_is_reported_as_a_warning() {
    let server = TestServer::start().await;
    server.seed(1).await;
    server.seed_without_raw(2).await;
    let out = server.out("partial.mbox");

    let output = server
        .ok(&["export", "", "-o", out.to_str().unwrap()])
        .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exported 1 message(s)"), "stderr: {stderr}");
    assert!(
        stderr.contains("no stored raw RFC822"),
        "the skipped message must be reported, not buried in a server log: {stderr}"
    );
    server.stop().await;
}

#[tokio::test]
async fn an_unenforceable_query_is_refused_and_writes_nothing() {
    let server = TestServer::start().await;
    server.seed(3).await;
    let out = server.out("widened.mbox");

    let stderr = server
        .fails(&["export", "~invoice", "-o", out.to_str().unwrap()])
        .await;
    assert!(stderr.contains("cannot enforce"), "stderr: {stderr}");
    assert!(
        !out.exists(),
        "a query the daemon cannot enforce must not become an archive of everything"
    );
    server.stop().await;
}

#[tokio::test]
async fn naming_neither_a_query_nor_a_thread_is_refused() {
    let server = TestServer::start().await;
    let out = server.out("nothing.mbox");
    let stderr = server
        .fails(&["export", "--format", "mbox", "-o", out.to_str().unwrap()])
        .await;
    assert!(stderr.contains("--thread"), "stderr: {stderr}");
    server.stop().await;
}
