//! A minimal in-process mock IMAP server for tests (no network, plaintext).
//!
//! It speaks just enough of the protocol to exercise login, capability probing,
//! folder listing, folder selection, UID fetching, and logout: it echoes the
//! command tag, accepts/rejects `LOGIN` against a configured password, and
//! serves a configured message set from `SELECT`/`UID FETCH`.
//!
//! It serves connections in a loop, so a test may connect more than once (the
//! resume/re-run sync cases do). Every `UID FETCH` set it receives is recorded,
//! so a test can assert that an incremental sync issued *no* fetches rather
//! than merely producing no new rows.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Configuration for a mock server run.
#[derive(Clone)]
pub(crate) struct MockConfig {
    password: String,
    /// `(name, attributes)` pairs returned by `LIST`.
    folders: Vec<(String, String)>,
    /// Messages per folder, served by `UID FETCH` after that folder is
    /// selected. Keyed by folder name so selecting the wrong folder is
    /// detectable.
    messages: std::collections::BTreeMap<String, Vec<FetchSpec>>,
    /// UIDVALIDITY reported by `SELECT`.
    uidvalidity: u32,
    /// Folders whose `SELECT` is answered with a tagged `NO`.
    unselectable: Vec<String>,
    /// Response codes to omit from `SELECT`, to exercise the error paths.
    omit_uidvalidity: bool,
    omit_uidnext: bool,
}

/// The folder messages default into when a test does not name one.
const DEFAULT_FOLDER: &str = "INBOX";

/// A canned message the mock returns for `UID FETCH`.
#[derive(Clone)]
pub(crate) struct FetchSpec {
    pub(crate) uid: u32,
    pub(crate) flags: Vec<String>,
    pub(crate) raw: Vec<u8>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            password: "password".to_owned(),
            folders: vec![("INBOX".to_owned(), String::new())],
            messages: std::collections::BTreeMap::new(),
            uidvalidity: 1,
            unselectable: Vec::new(),
            omit_uidvalidity: false,
            omit_uidnext: false,
        }
    }
}

impl MockConfig {
    /// Set the accepted password.
    pub(crate) fn password(mut self, password: &str) -> Self {
        self.password = password.to_owned();
        self
    }

    /// Set the folders returned by `LIST`.
    pub(crate) fn folders(mut self, folders: Vec<(&str, &str)>) -> Self {
        self.folders = folders
            .into_iter()
            .map(|(n, a)| (n.to_owned(), a.to_owned()))
            .collect();
        self
    }

    /// Set the UIDVALIDITY reported by `SELECT`.
    pub(crate) fn uidvalidity(mut self, uidvalidity: u32) -> Self {
        self.uidvalidity = uidvalidity;
        self
    }

    /// Add one message to the default folder.
    pub(crate) fn fetch(self, uid: u32, flags: &[&str], raw: &[u8]) -> Self {
        self.fetch_in(DEFAULT_FOLDER, uid, flags, raw)
    }

    /// Add one message to a named folder.
    pub(crate) fn fetch_in(mut self, folder: &str, uid: u32, flags: &[&str], raw: &[u8]) -> Self {
        let entry = self.messages.entry(folder.to_owned()).or_default();
        entry.push(FetchSpec {
            uid,
            flags: flags.iter().map(|f| (*f).to_owned()).collect(),
            raw: raw.to_vec(),
        });
        entry.sort_by_key(|m| m.uid);
        self
    }

    /// Answer `SELECT` for this folder with a tagged `NO`.
    pub(crate) fn unselectable(mut self, folder: &str) -> Self {
        self.unselectable.push(folder.to_owned());
        self
    }

    /// Omit the `UIDVALIDITY` response code from `SELECT`.
    pub(crate) fn without_uidvalidity(mut self) -> Self {
        self.omit_uidvalidity = true;
        self
    }

    /// Omit the `UIDNEXT` response code from `SELECT`.
    pub(crate) fn without_uidnext(mut self) -> Self {
        self.omit_uidnext = true;
        self
    }

    /// The messages in a folder.
    fn folder_messages(&self, folder: &str) -> &[FetchSpec] {
        self.messages.get(folder).map_or(&[], Vec::as_slice)
    }

    /// The highest UID in a folder, or 0 when it is empty.
    fn max_uid(&self, folder: &str) -> u32 {
        self.folder_messages(folder).last().map_or(0, |m| m.uid)
    }
}

/// A running mock server bound to an ephemeral loopback port.
pub(crate) struct MockImap {
    /// Address to connect to.
    pub(crate) addr: SocketAddr,
    /// Every `UID FETCH` argument string the server received.
    fetch_log: Arc<Mutex<Vec<String>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockImap {
    /// Bind and start serving connections until dropped.
    pub(crate) async fn start(config: MockConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fetch_log = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&fetch_log);
        let handle = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let config = config.clone();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let _ = serve(sock, config, log).await;
                });
            }
        });
        Self {
            addr,
            fetch_log,
            _handle: handle,
        }
    }

    /// The `UID FETCH` sets requested so far, in order.
    ///
    /// Panics if a serve task poisoned the log — a test asserting "no FETCH was
    /// sent" must fail loudly rather than pass on an empty vector.
    pub(crate) fn fetch_commands(&self) -> Vec<String> {
        self.fetch_log
            .lock()
            .expect("mock serve task panicked")
            .clone()
    }
}

impl Drop for MockImap {
    fn drop(&mut self) {
        // A dropped JoinHandle detaches rather than aborts, which would leak the
        // listener task for the life of the test binary.
        self._handle.abort();
    }
}

/// Expand an IMAP UID set (`5`, `1:10`, `3,7:9`, `*`, `10:*`) against the
/// highest present UID.
fn uids_in_set(set: &str, max_uid: u32) -> Vec<u32> {
    let resolve = |token: &str| -> Option<u32> {
        if token == "*" {
            Some(max_uid)
        } else {
            token.parse().ok()
        }
    };
    let mut uids = Vec::new();
    for part in set.split(',') {
        let part = part.trim();
        match part.split_once(':') {
            Some((lo, hi)) => {
                let (Some(lo), Some(hi)) = (resolve(lo), resolve(hi)) else {
                    continue;
                };
                let (lo, hi) = (lo.min(hi), lo.max(hi));
                uids.extend(lo..=hi);
            }
            None => uids.extend(resolve(part)),
        }
    }
    uids
}

async fn serve(
    sock: TcpStream,
    config: MockConfig,
    fetch_log: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (read_half, mut write) = sock.into_split();
    let mut reader = BufReader::new(read_half);

    write
        .write_all(b"* OK [CAPABILITY IMAP4rev1] rmail mock ready\r\n")
        .await?;

    let mut selected = DEFAULT_FOLDER.to_owned();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let mut parts = trimmed.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        let command = parts.next().unwrap_or("");
        let verb = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();

        match verb.as_str() {
            "LOGIN" => {
                let supplied = command
                    .split_whitespace()
                    .last()
                    .map(|p| p.trim_matches('"'))
                    .unwrap_or("");
                if supplied == config.password {
                    write
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await?;
                } else {
                    write
                        .write_all(
                            format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n")
                                .as_bytes(),
                        )
                        .await?;
                }
            }
            "CAPABILITY" => {
                write
                    .write_all(b"* CAPABILITY IMAP4rev1 IDLE CONDSTORE QRESYNC MOVE\r\n")
                    .await?;
                write
                    .write_all(format!("{tag} OK CAPABILITY completed\r\n").as_bytes())
                    .await?;
            }
            "LIST" => {
                for (name, attrs) in &config.folders {
                    write
                        .write_all(format!("* LIST ({attrs}) \"/\" \"{name}\"\r\n").as_bytes())
                        .await?;
                }
                write
                    .write_all(format!("{tag} OK LIST completed\r\n").as_bytes())
                    .await?;
            }
            "SELECT" | "EXAMINE" => {
                let name = command
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_owned();
                if config.unselectable.iter().any(|f| f == &name) {
                    write
                        .write_all(format!("{tag} NO Mailbox doesn't exist: {name}\r\n").as_bytes())
                        .await?;
                    continue;
                }
                selected = name.clone();
                let exists = config.folder_messages(&name).len();
                let uidnext = config.max_uid(&name) + 1;
                write
                    .write_all(format!("* {exists} EXISTS\r\n* 0 RECENT\r\n").as_bytes())
                    .await?;
                if !config.omit_uidvalidity {
                    write
                        .write_all(
                            format!("* OK [UIDVALIDITY {}] UIDs valid\r\n", config.uidvalidity)
                                .as_bytes(),
                        )
                        .await?;
                }
                if !config.omit_uidnext {
                    write
                        .write_all(
                            format!("* OK [UIDNEXT {uidnext}] Predicted next UID\r\n").as_bytes(),
                        )
                        .await?;
                }
                write
                    .write_all(format!("{tag} OK [READ-WRITE] {verb} completed\r\n").as_bytes())
                    .await?;
            }
            "UID" => {
                // Only `UID FETCH <set> <attrs>` is modeled.
                let mut args = command.split_whitespace().skip(1);
                let sub = args.next().unwrap_or("").to_ascii_uppercase();
                let set = args.next().unwrap_or("").to_owned();
                if sub == "FETCH" {
                    if let Ok(mut log) = fetch_log.lock() {
                        log.push(set.clone());
                    }
                    let messages = config.folder_messages(&selected);
                    let wanted = uids_in_set(&set, config.max_uid(&selected));
                    for (seq, spec) in messages.iter().enumerate() {
                        if !wanted.contains(&spec.uid) {
                            continue;
                        }
                        let raw = &spec.raw;
                        let n = raw.len();
                        let flags = spec.flags.join(" ");
                        let head = format!(
                            "* {} FETCH (UID {} FLAGS ({flags}) \
                             INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" \
                             RFC822.SIZE {n} BODY[] {{{n}}}\r\n",
                            seq + 1,
                            spec.uid
                        );
                        write.write_all(head.as_bytes()).await?;
                        write.write_all(raw).await?;
                        write.write_all(b")\r\n").await?;
                    }
                }
                write
                    .write_all(format!("{tag} OK UID {sub} completed\r\n").as_bytes())
                    .await?;
            }
            "LOGOUT" => {
                write.write_all(b"* BYE logging out\r\n").await?;
                write
                    .write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes())
                    .await?;
                break;
            }
            _ => {
                write.write_all(format!("{tag} OK\r\n").as_bytes()).await?;
            }
        }
    }
    Ok(())
}
