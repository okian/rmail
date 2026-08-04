//! Shared scaffolding for the delta-sync suites ([`super::qresync`] and
//! [`super::uiddiff_fallback`]).
//!
//! Both suites tell the same story from different servers: sync a folder, let
//! the server change underneath it, and check that only the change moved. The
//! pieces they share are the temp database, the mock server wiring, and the
//! assertions about what actually landed on disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use async_imap::Session;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::imap::conn::{login, probe_capabilities};
use crate::imap::mock::{MockConfig, MockImap};
use crate::imap::ImapCapabilities;
use crate::repo;
use crate::storage::Database;
use crate::sync::{full, SyncOptions};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The UIDVALIDITY every fixture server starts in.
pub(super) const UIDVALIDITY: i64 = 42;

/// Build a distinct message for `uid` so every row is independently checkable.
pub(super) fn raw(uid: u32) -> Vec<u8> {
    format!(
        "From: sender{uid}@example.com\r\n\
         To: me@example.com\r\n\
         Subject: Message {uid}\r\n\
         Message-ID: <m{uid}@example.com>\r\n\
         Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
         \r\n\
         body {uid}\r\n"
    )
    .into_bytes()
}

/// A mock server holding messages with UIDs `1..=count`, all at modseq 1.
pub(super) fn mock_config(count: u32) -> MockConfig {
    let mut config = MockConfig::default()
        .password("pw")
        .uidvalidity(u32::try_from(UIDVALIDITY).unwrap());
    for uid in 1..=count {
        config = config.fetch(uid, &["\\Seen"], &raw(uid));
    }
    config
}

/// A temp database with one account and one `INBOX` mailbox.
pub(super) struct Fixture {
    pub(super) db: Database,
    pub(super) mailbox_id: i64,
    path: PathBuf,
}

impl Fixture {
    pub(super) async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-syncdelta-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let mailbox_id = db
            .write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        Self {
            db,
            mailbox_id,
            path,
        }
    }

    /// The UIDs stored for this mailbox in the fixture UID space, ascending.
    pub(super) fn stored_uids(&self) -> Vec<i64> {
        self.uids_at(UIDVALIDITY)
    }

    /// The UIDs stored at an explicit UIDVALIDITY.
    pub(super) fn uids_at(&self, uidvalidity: i64) -> Vec<i64> {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| repo::list_message_uids(c, mailbox_id, uidvalidity, 1, i64::MAX))
            .unwrap()
    }

    /// The flags stored for a UID, sorted.
    pub(super) fn flags_of(&self, uid: i64) -> Vec<String> {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| {
                let message = repo::get_message_by_identity(c, mailbox_id, UIDVALIDITY, uid)?
                    .expect("message should be stored");
                repo::list_flags(c, message.id)
            })
            .unwrap()
    }

    pub(super) fn message_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)))
            .unwrap()
    }

    pub(super) fn thread_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM threads", [], |r| r.get(0)))
            .unwrap()
    }

    pub(super) fn sync_state(&self) -> repo::SyncState {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| repo::get_sync_state(c, mailbox_id))
            .unwrap()
            .expect("checkpoint written")
    }

    /// Run the initial UID-window walk against `mock`, the way a folder becomes
    /// delta-syncable in the first place.
    pub(super) async fn full_sync(&self, mock: &MockImap) {
        let mut session = connect(mock).await;
        full::sync_folder(
            &mut session,
            &self.db,
            self.mailbox_id,
            SyncOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        let _ = session.logout().await;
    }

    /// Delta-sync against `mock` with the capabilities it advertises.
    pub(super) async fn delta(&self, mock: &MockImap) -> super::DeltaReport {
        self.delta_with(mock, &CancellationToken::new()).await
    }

    /// Delta-sync against `mock` under a caller-owned cancellation token.
    pub(super) async fn delta_with(
        &self,
        mock: &MockImap,
        cancel: &CancellationToken,
    ) -> super::DeltaReport {
        let (mut session, capabilities) = connect_with_capabilities(mock).await;
        let report = self
            .delta_on(&mut session, capabilities, cancel)
            .await
            .unwrap();
        let _ = session.logout().await;
        report
    }

    /// Delta-sync against `mock` while claiming capabilities it may not honor —
    /// how a server that advertises an extension and then refuses it is driven.
    pub(super) async fn delta_claiming(
        &self,
        mock: &MockImap,
        capabilities: ImapCapabilities,
    ) -> super::DeltaReport {
        let mut session = connect(mock).await;
        let report = self
            .delta_on(&mut session, capabilities, &CancellationToken::new())
            .await
            .unwrap();
        let _ = session.logout().await;
        report
    }

    /// Delta-sync on a caller-owned session, returning the raw result so error
    /// paths can be asserted on.
    ///
    /// Enables QRESYNC first, exactly as `delta_sync_folders` does in
    /// production — it is a session-level command, not a per-folder one.
    pub(super) async fn delta_on<T>(
        &self,
        session: &mut Session<T>,
        capabilities: ImapCapabilities,
        cancel: &CancellationToken,
    ) -> Result<super::DeltaReport, crate::Error>
    where
        T: crate::imap::conn::ImapStream,
    {
        let capabilities = super::delta::enable_qresync(
            session,
            capabilities,
            SyncOptions::default().window_timeout,
        )
        .await;
        super::delta_sync(
            session,
            &self.db,
            self.mailbox_id,
            capabilities,
            SyncOptions::default(),
            cancel,
        )
        .await
    }

    /// The `mailboxes` row, which mirrors what the last `SELECT` reported.
    pub(super) fn mailbox_row(&self) -> repo::Mailbox {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| repo::get_mailbox(c, mailbox_id))
            .unwrap()
            .expect("mailbox row")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Log in to a mock server.
pub(super) async fn connect(mock: &MockImap) -> Session<TcpStream> {
    let stream = TcpStream::connect(mock.addr).await.unwrap();
    login(stream, "user", "pw").await.unwrap()
}

/// Log in and probe what the server says it can do, as the daemon does.
pub(super) async fn connect_with_capabilities(
    mock: &MockImap,
) -> (Session<TcpStream>, ImapCapabilities) {
    let mut session = connect(mock).await;
    let capabilities = probe_capabilities(&mut session).await.unwrap();
    (session, capabilities)
}

/// The `UID FETCH` commands `mock` received that carry a message body — the
/// expensive ones a delta is supposed to avoid.
pub(super) fn body_fetches(mock: &MockImap) -> Vec<String> {
    mock.commands()
        .into_iter()
        .filter(|command| {
            let upper = command.to_ascii_uppercase();
            upper.starts_with("UID FETCH") && upper.contains("BODY[")
        })
        .collect()
}

/// The commands `mock` received whose verb matches `prefix`.
pub(super) fn commands_starting(mock: &MockImap, prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_uppercase();
    mock.commands()
        .into_iter()
        .filter(|command| command.to_ascii_uppercase().starts_with(&prefix))
        .collect()
}
