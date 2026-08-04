//! IMAP connectivity: connect over TLS, log in, probe capabilities, and list
//! folders.
//!
//! [`test_connection`] is the account-level entry point: it resolves the
//! account's credential, logs in over `rustls` TLS, probes capabilities,
//! discovers folders into the `mailboxes` table, and returns a
//! [`ConnectionReport`]. Failures map to the domain error model — a rejected
//! login is `UNAUTHENTICATED`, an unreachable/broken connection is
//! `UNAVAILABLE`, and an unconfigured account is `FAILED_PRECONDITION` — so the
//! daemon stays up and local features keep working.

pub mod conn;
pub mod folders;

#[cfg(test)]
pub(crate) mod mock;

use std::time::Duration;

use crate::error::Error;
use crate::storage::Database;

/// Overall deadline for a connection test's network exchange, so a wedged
/// server cannot hang the RPC indefinitely.
pub const IMAP_DEADLINE: Duration = Duration::from_secs(30);

/// The IMAP capabilities rmail cares about, probed after login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImapCapabilities {
    /// `IDLE` — push notifications (task 13).
    pub idle: bool,
    /// `CONDSTORE` — modseq-based change tracking, used by
    /// [`crate::sync::delta`].
    pub condstore: bool,
    /// `QRESYNC` — quick resynchronization, used by [`crate::sync::delta`].
    pub qresync: bool,
    /// `MOVE` — atomic message move.
    pub move_: bool,
}

impl ImapCapabilities {
    /// The same capabilities with the modseq extensions masked off.
    ///
    /// This is how `sync.qresync = false` reaches the delta engine: the engine
    /// is told what it may use, not what the server happens to advertise, so
    /// turning the setting off downgrades a run to the UID-enumeration diff
    /// instead of being quietly ignored. Some servers advertise CONDSTORE and
    /// then report modseqs that go backwards — being able to switch it off per
    /// account is the difference between a workaround and a re-sync.
    #[must_use]
    pub fn without_modseq(self) -> Self {
        Self {
            condstore: false,
            qresync: false,
            ..self
        }
    }
}

/// One discovered folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    /// Full IMAP folder name.
    pub name: String,
    /// Hierarchy delimiter, if the server reported one.
    pub delimiter: Option<String>,
    /// Whether the folder can be `SELECT`ed (false if `\Noselect`).
    pub selectable: bool,
}

/// The result of a successful connection test.
#[derive(Debug, Clone)]
pub struct ConnectionReport {
    /// Probed server capabilities.
    pub capabilities: ImapCapabilities,
    /// Discovered folders (also persisted to `mailboxes`).
    pub folders: Vec<FolderInfo>,
}

/// Map an `async-imap` error to the domain error model.
///
/// A `NO`/authentication response is `UNAUTHENTICATED`; a `Validate` error is
/// `INVALID_ARGUMENT`; I/O and protocol errors are `UNAVAILABLE` (retryable).
pub(crate) fn map_imap_err(err: async_imap::error::Error) -> Error {
    use async_imap::error::Error as E;
    match err {
        E::No(msg) => Error::unauthenticated(format!("IMAP login rejected: {msg}")),
        E::Validate(_) => Error::invalid_argument("invalid IMAP username or password"),
        E::Io(e) => Error::unavailable(format!("IMAP connection error: {e}")),
        E::ConnectionLost => Error::unavailable("IMAP connection lost"),
        // Bad/Parse/Append and any future non-exhaustive variant: treat as a
        // (retryable) server/protocol problem.
        other => Error::unavailable(format!("IMAP protocol error: {other}")),
    }
}

/// Map a failure of a data command (`UID FETCH`, `UID SEARCH`) named by `op`.
///
/// [`map_imap_err`] is login-shaped — it reads a tagged `NO` as rejected
/// credentials. On a fetch or a search a `NO` means something entirely
/// different and entirely routine (`NO [LIMIT]`, `NO Server busy`, `NO Some
/// messages could not be fetched`), and reporting it as `UNAUTHENTICATED`
/// sends whoever is on call chasing an authentication problem that does not
/// exist. It is a retryable server refusal: `UNAVAILABLE`.
pub(crate) fn command_error(op: &str, err: async_imap::error::Error) -> Error {
    match err {
        async_imap::error::Error::No(msg) => {
            Error::unavailable(format!("IMAP {op} refused: {msg}"))
        }
        other => map_imap_err(other),
    }
}

/// Connect to `account_id`'s IMAP server, verify login, probe capabilities, and
/// discover folders into `mailboxes`.
///
/// # Errors
///
/// - [`Error::FailedPrecondition`] if the account has no IMAP server or no
///   credential configured.
/// - [`Error::Unauthenticated`] if the credential cannot be resolved or the
///   server rejects the login.
/// - [`Error::Unavailable`] if the server is unreachable or the connection
///   breaks.
#[tracing::instrument(skip(db), err)]
pub async fn test_connection(db: &Database, account_id: i64) -> Result<ConnectionReport, Error> {
    let account = crate::account::get(db, account_id).await?;

    let host = account
        .imap_server
        .clone()
        .ok_or_else(|| Error::failed_precondition("account has no IMAP server configured"))?;
    let port = account.imap_port.unwrap_or(993);
    let username = account.username.clone().unwrap_or_default();

    // Resolve the credential on a blocking thread (a password_command or
    // keychain lookup may block); the secret never leaves this scope's caller.
    let credential = account.credential.clone();
    let username_for_resolve = account.username.clone();
    let secret =
        tokio::task::spawn_blocking(move || credential.resolve(username_for_resolve.as_deref()))
            .await
            .map_err(|e| Error::internal(format!("credential resolution task failed: {e}")))??
            .ok_or_else(|| Error::failed_precondition("account has no credential configured"))?;

    tracing::info!(host = %host, port, "testing IMAP connection");

    // Bound the whole network exchange so a wedged server can't hang the RPC.
    let report = tokio::time::timeout(IMAP_DEADLINE, async {
        let stream = conn::connect_tls(&host, port).await?;
        run_session(db, account_id, &username, secret.expose(), stream).await
    })
    .await
    .map_err(|_| Error::deadline_exceeded("IMAP connection test timed out"))??;

    tracing::info!(
        folder_count = report.folders.len(),
        idle = report.capabilities.idle,
        "IMAP connection verified"
    );
    Ok(report)
}

/// Run the post-connect exchange on an established stream: login, probe
/// capabilities, list folders, persist them, and log out. Generic over the
/// stream so tests can drive it with the in-process mock.
///
/// # Errors
///
/// [`Error::Unauthenticated`] on login rejection; [`Error::Unavailable`] on a
/// broken connection; otherwise a mapped storage error.
pub(crate) async fn run_session<S: conn::ImapStream>(
    db: &Database,
    account_id: i64,
    username: &str,
    password: &str,
    stream: S,
) -> Result<ConnectionReport, Error> {
    let mut session = conn::login(stream, username, password).await?;
    let capabilities = conn::probe_capabilities(&mut session).await?;
    let folders = folders::list_folders(&mut session).await?;
    folders::store_folders(db, account_id, &folders).await?;

    // Best-effort logout; the connection is dropped regardless.
    let _ = session.logout().await;

    Ok(ConnectionReport {
        capabilities,
        folders,
    })
}
