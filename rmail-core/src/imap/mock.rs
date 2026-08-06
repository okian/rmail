//! A minimal in-process mock IMAP server for tests (no network, plaintext).
//!
//! It speaks just enough of the protocol to exercise login, capability probing,
//! folder listing, folder selection, UID fetching, UID search, `ENABLE`, and
//! logout: it echoes the command tag, accepts/rejects `LOGIN` against a
//! configured password, and serves a configured message set.
//!
//! It also models the modseq world the delta sync lives in — per-message
//! `MODSEQ`, a folder `HIGHESTMODSEQ`, `CHANGEDSINCE` filtering, and `VANISHED
//! (EARLIER)` for messages expunged at a known modseq — and its advertised
//! capability list is configurable, so the CONDSTORE-less fallback can be
//! driven against a server that genuinely lacks the extension rather than one
//! that has been asked politely not to use it.
//!
//! It serves connections in a loop, so a test may connect more than once (the
//! resume/re-run sync cases do). Every command it receives is recorded, so a
//! test can assert that an incremental sync issued *no* fetches — or that it
//! issued exactly the CONDSTORE one — rather than merely producing no new rows.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Capabilities advertised unless a test narrows them.
const DEFAULT_CAPABILITIES: &[&str] = &["IMAP4rev1", "IDLE", "CONDSTORE", "QRESYNC", "MOVE"];

/// The folder messages default into when a test does not name one.
const DEFAULT_FOLDER: &str = "INBOX";

/// Configuration for a mock server run.
#[derive(Clone)]
pub(crate) struct MockConfig {
    password: String,
    /// `(name, attributes)` pairs returned by `LIST`.
    folders: Vec<(String, String)>,
    /// Messages per folder, served by `UID FETCH` after that folder is
    /// selected. Keyed by folder name so selecting the wrong folder is
    /// detectable.
    messages: BTreeMap<String, Vec<FetchSpec>>,
    /// `(uid, modseq)` for messages expunged from a folder, reported as
    /// `VANISHED (EARLIER)` to a `CHANGEDSINCE` fetch that asks for it.
    expunged: BTreeMap<String, Vec<(u32, u64)>>,
    /// Capabilities advertised in the greeting and by `CAPABILITY`.
    capabilities: Vec<String>,
    /// UIDVALIDITY reported by `SELECT`.
    uidvalidity: u32,
    /// Folders whose `SELECT` is answered with a tagged `NO`.
    unselectable: Vec<String>,
    /// Response codes to omit from `SELECT`, to exercise the error paths.
    omit_uidvalidity: bool,
    omit_uidnext: bool,
    /// Answer every `UID SEARCH` with an empty set regardless of content.
    empty_search: bool,
    /// Answer every `UID` command with a tagged `NO`.
    refuse_uid: bool,
    /// Answer every `UID` command with a tagged `NO [TRYCREATE]` — the one
    /// refusal RFC 3501 defines as "the destination does not exist".
    refuse_uid_trycreate: bool,
    /// How often to volunteer `* OK Still here` while idling.
    idle_keepalive: Duration,
}

/// A canned message the mock returns for `UID FETCH`.
#[derive(Clone)]
pub(crate) struct FetchSpec {
    pub(crate) uid: u32,
    pub(crate) flags: Vec<String>,
    pub(crate) raw: Vec<u8>,
    /// The modseq at which this message last changed.
    pub(crate) modseq: u64,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            password: "password".to_owned(),
            folders: vec![("INBOX".to_owned(), String::new())],
            messages: BTreeMap::new(),
            expunged: BTreeMap::new(),
            capabilities: DEFAULT_CAPABILITIES
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
            uidvalidity: 1,
            unselectable: Vec::new(),
            omit_uidvalidity: false,
            omit_uidnext: false,
            empty_search: false,
            refuse_uid: false,
            refuse_uid_trycreate: false,
            // Effectively never, unless a test asks for it.
            idle_keepalive: Duration::from_secs(86_400),
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

    /// Replace the advertised capability list.
    pub(crate) fn capabilities(mut self, capabilities: &[&str]) -> Self {
        self.capabilities = capabilities.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    /// Add one message to the default folder at modseq 1.
    pub(crate) fn fetch(self, uid: u32, flags: &[&str], raw: &[u8]) -> Self {
        self.fetch_in(DEFAULT_FOLDER, uid, flags, raw)
    }

    /// Add one message to a named folder at modseq 1.
    pub(crate) fn fetch_in(self, folder: &str, uid: u32, flags: &[&str], raw: &[u8]) -> Self {
        self.fetch_at(folder, uid, flags, raw, 1)
    }

    /// Add one message to a named folder at an explicit modseq.
    pub(crate) fn fetch_at(
        mut self,
        folder: &str,
        uid: u32,
        flags: &[&str],
        raw: &[u8],
        modseq: u64,
    ) -> Self {
        let entry = self.messages.entry(folder.to_owned()).or_default();
        entry.push(FetchSpec {
            uid,
            flags: flags.iter().map(|f| (*f).to_owned()).collect(),
            raw: raw.to_vec(),
            modseq,
        });
        entry.sort_by_key(|m| m.uid);
        self
    }

    /// Change an existing message's flags and modseq, leaving its body alone —
    /// what a flag flip on another device looks like from the server side.
    pub(crate) fn change(mut self, uid: u32, flags: &[&str], modseq: u64) -> Self {
        let spec = self
            .messages
            .get_mut(DEFAULT_FOLDER)
            .and_then(|specs| specs.iter_mut().find(|spec| spec.uid == uid))
            .expect("cannot change a message the mock does not hold");
        spec.flags = flags.iter().map(|f| (*f).to_owned()).collect();
        spec.modseq = modseq;
        self
    }

    /// Record a message expunged from the default folder at `modseq`.
    pub(crate) fn expunged(self, uid: u32, modseq: u64) -> Self {
        self.expunged_in(DEFAULT_FOLDER, uid, modseq)
    }

    /// Record a message expunged from a named folder at `modseq`.
    pub(crate) fn expunged_in(mut self, folder: &str, uid: u32, modseq: u64) -> Self {
        self.expunged
            .entry(folder.to_owned())
            .or_default()
            .push((uid, modseq));
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

    /// Answer every `UID SEARCH` with an empty set, however many messages the
    /// folder holds — a server having a bad day, and the one answer a client
    /// must never act on.
    pub(crate) fn with_broken_search(mut self) -> Self {
        self.empty_search = true;
        self
    }

    /// Volunteer `* OK Still here` this often while idling, as Dovecot, Cyrus
    /// and Gmail all do. A client that treats every server response as a reason
    /// to restart its own timer will never reissue `IDLE` against such a
    /// server — which is the failure this exists to catch.
    pub(crate) fn idle_keepalive(mut self, every: Duration) -> Self {
        self.idle_keepalive = every;
        self
    }

    /// Answer every `UID` command with a tagged `NO`, the way a real server
    /// refuses one it cannot serve right now (`NO [LIMIT]`, `NO Server busy`).
    /// Nothing to do with credentials — which is the whole point.
    pub(crate) fn refusing_uid_commands(mut self) -> Self {
        self.refuse_uid = true;
        self
    }

    /// Refuse every `UID` command with `NO [TRYCREATE]`.
    ///
    /// The counterpart to [`Self::refusing_uid_commands`]: that one emulates a
    /// server declining work it could otherwise do (`[LIMIT]`, `Server busy`),
    /// which is transient; this one emulates the single refusal RFC 3501
    /// defines as meaning the destination mailbox does not exist, which is
    /// permanent. `COPY`/`MOVE` error mapping has to tell them apart, so the
    /// mock has to be able to produce both.
    pub(crate) fn refusing_uid_commands_with_trycreate(mut self) -> Self {
        self.refuse_uid_trycreate = true;
        self
    }

    /// The messages in a folder.
    fn folder_messages(&self, folder: &str) -> &[FetchSpec] {
        self.messages.get(folder).map_or(&[], Vec::as_slice)
    }

    /// The messages expunged from a folder.
    fn folder_expunged(&self, folder: &str) -> &[(u32, u64)] {
        self.expunged.get(folder).map_or(&[], Vec::as_slice)
    }

    /// The highest live UID in a folder, or 0 when it has none.
    fn max_uid(&self, folder: &str) -> u32 {
        self.folder_messages(folder).last().map_or(0, |m| m.uid)
    }

    /// The highest UID the folder's space has ever held, live or expunged —
    /// what `UIDNEXT - 1` must sit at, so an expunge does not make the ceiling
    /// move backwards.
    fn ceiling(&self, folder: &str) -> u32 {
        let expunged = self
            .folder_expunged(folder)
            .iter()
            .map(|(uid, _)| *uid)
            .max()
            .unwrap_or(0);
        self.max_uid(folder).max(expunged)
    }

    /// The folder's `HIGHESTMODSEQ`: the newest change it has seen, counting
    /// expunges.
    fn highest_modseq(&self, folder: &str) -> u64 {
        let messages = self
            .folder_messages(folder)
            .iter()
            .map(|m| m.modseq)
            .max()
            .unwrap_or(0);
        let expunged = self
            .folder_expunged(folder)
            .iter()
            .map(|(_, modseq)| *modseq)
            .max()
            .unwrap_or(0);
        messages.max(expunged).max(1)
    }

    /// Whether the advertised capability list contains `name`.
    fn advertises(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
    }
}

/// A running mock server bound to an ephemeral loopback port.
pub(crate) struct MockImap {
    /// Address to connect to.
    pub(crate) addr: SocketAddr,
    /// Every command line the server received, tag stripped.
    command_log: Arc<Mutex<Vec<String>>>,
    /// Untagged lines a test wants pushed to every idling connection.
    pushes: broadcast::Sender<String>,
    /// Connections currently parked on `IDLE`.
    idling: Arc<AtomicUsize>,
    /// Cancelled on drop, which closes every live connection.
    shutdown: CancellationToken,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockImap {
    /// Bind and start serving connections until dropped.
    pub(crate) async fn start(config: MockConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let command_log = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&command_log);
        let (pushes, _) = broadcast::channel(64);
        let push_tx = pushes.clone();
        let shutdown = CancellationToken::new();
        let serve_shutdown = shutdown.clone();
        let idling = Arc::new(AtomicUsize::new(0));
        let serve_idling = Arc::clone(&idling);
        let handle = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let config = config.clone();
                let log = Arc::clone(&log);
                let pushes = push_tx.subscribe();
                let shutdown = serve_shutdown.clone();
                let idling = Arc::clone(&serve_idling);
                tokio::spawn(async move {
                    let _ = serve(sock, config, log, pushes, shutdown, idling).await;
                });
            }
        });
        Self {
            addr,
            command_log,
            pushes,
            idling,
            shutdown,
            _handle: handle,
        }
    }

    /// Push an untagged line to every connection currently idling.
    ///
    /// This is how a test plays the part of "someone else touched the mailbox"
    /// — the server volunteering `* 4 EXISTS` is exactly what IDLE exists to
    /// deliver, and nothing about it is observable from the client's commands.
    pub(crate) fn push(&self, line: &str) {
        // An error means nobody is subscribed yet; the test's own assertions
        // cover that case far better than a panic here would.
        let _ = self.pushes.send(line.to_owned());
    }

    /// Whether any connection is parked on `IDLE` right now.
    ///
    /// Counted where `+ idling` is actually written, not where a connection is
    /// accepted — a gate that means "someone connected" would let a test claim
    /// to have cancelled a parked watch when it had only cancelled a connected
    /// one.
    pub(crate) fn idling(&self) -> bool {
        self.idling.load(Ordering::SeqCst) > 0
    }

    /// Every command received so far, in order, without its tag.
    ///
    /// Panics if a serve task poisoned the log — a test asserting "no FETCH was
    /// sent" must fail loudly rather than pass on an empty vector.
    pub(crate) fn commands(&self) -> Vec<String> {
        self.command_log
            .lock()
            .expect("mock serve task panicked")
            .clone()
    }

    /// The `UID FETCH` sets requested so far, in order.
    pub(crate) fn fetch_commands(&self) -> Vec<String> {
        self.commands()
            .iter()
            .filter_map(|command| {
                let rest = strip_prefix_ignore_case(command, "UID FETCH ")?;
                Some(rest.split_whitespace().next().unwrap_or("").to_owned())
            })
            .collect()
    }
}

impl Drop for MockImap {
    fn drop(&mut self) {
        // Aborting the accept loop stops *new* connections; the ones already
        // being served are separate tasks and would otherwise keep answering
        // long after the server they belong to is gone. A test that drops a
        // mock to simulate an outage needs the sockets to actually die.
        self.shutdown.cancel();
        // A dropped JoinHandle detaches rather than aborts, which would leak the
        // listener task for the life of the test binary.
        self._handle.abort();
    }
}

/// Case-insensitive [`str::strip_prefix`].
fn strip_prefix_ignore_case<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    haystack
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &haystack[prefix.len()..])
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

/// The `CHANGEDSINCE`/`VANISHED` modifiers of a `UID FETCH`, if present.
#[derive(Default)]
struct FetchModifiers {
    changedsince: Option<u64>,
    vanished: bool,
    /// Whether the requested attributes include the message body.
    wants_body: bool,
}

/// Parse the attribute/modifier tail of a `UID FETCH <set> …` command.
fn parse_fetch_modifiers(tail: &str) -> FetchModifiers {
    let upper = tail.to_ascii_uppercase();
    let changedsince = upper.find("CHANGEDSINCE").and_then(|at| {
        upper[at + "CHANGEDSINCE".len()..]
            .split(|c: char| !c.is_ascii_digit())
            .find(|token| !token.is_empty())
            .and_then(|token| token.parse().ok())
    });
    FetchModifiers {
        changedsince,
        vanished: upper.contains("VANISHED"),
        wants_body: upper.contains("BODY[") || upper.contains("RFC822"),
    }
}

/// Render an ascending UID list as a compact IMAP set (`1:3,7`).
fn render_uid_set(uids: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = uids.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;
    for uid in iter {
        if uid == end + 1 {
            end = uid;
        } else {
            parts.push(render_range(start, end));
            start = uid;
            end = uid;
        }
    }
    parts.push(render_range(start, end));
    parts.join(",")
}

fn render_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

async fn serve(
    sock: TcpStream,
    config: MockConfig,
    command_log: Arc<Mutex<Vec<String>>>,
    mut pushes: broadcast::Receiver<String>,
    shutdown: CancellationToken,
    idling: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (read_half, mut write) = sock.into_split();
    // Lines are read by a dedicated task and delivered over a channel, because
    // `AsyncBufReadExt::read_line` is not cancellation safe: dropping it
    // mid-line — which any `select!` will do — loses the bytes it had already
    // consumed. `mpsc::Receiver::recv` is safe to race, so every wait below can
    // be.
    let (line_tx, mut lines) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let capabilities = config.capabilities.join(" ");
    write
        .write_all(format!("* OK [CAPABILITY {capabilities}] rmail mock ready\r\n").as_bytes())
        .await?;

    // None until a mailbox is selected — the authenticated/selected state
    // distinction ENABLE depends on.
    let mut selected: Option<String> = None;
    let mut qresync_enabled = false;
    loop {
        let line = tokio::select! {
            line = lines.recv() => line,
            () = shutdown.cancelled() => return Ok(()),
        };
        let Some(line) = line else {
            break;
        };
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let mut parts = trimmed.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        let command = parts.next().unwrap_or("");
        if let Ok(mut log) = command_log.lock() {
            log.push(command.to_owned());
        }
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
                    .write_all(format!("* CAPABILITY {capabilities}\r\n").as_bytes())
                    .await?;
                write
                    .write_all(format!("{tag} OK CAPABILITY completed\r\n").as_bytes())
                    .await?;
            }
            "ENABLE" => {
                // RFC 5161 §3.1: ENABLE is only valid in the authenticated
                // state, with no mailbox selected. Enforcing that is the whole
                // point of modelling it — a client that issues ENABLE per
                // folder works fine against a lax server and silently loses the
                // extension against a strict one.
                if selected.is_some() {
                    write
                        .write_all(
                            format!("{tag} BAD ENABLE not permitted in selected state\r\n")
                                .as_bytes(),
                        )
                        .await?;
                    continue;
                }
                // Only extensions the server actually advertises can be enabled;
                // anything else gets a tagged NO, which is what a client that
                // guessed wrong must cope with.
                let wanted: Vec<&str> = command.split_whitespace().skip(1).collect();
                let known: Vec<&str> = wanted
                    .iter()
                    .copied()
                    .filter(|name| config.advertises(name))
                    .collect();
                if known.len() == wanted.len() && !known.is_empty() {
                    qresync_enabled |= known.iter().any(|n| n.eq_ignore_ascii_case("QRESYNC"));
                    write
                        .write_all(format!("* ENABLED {}\r\n", known.join(" ")).as_bytes())
                        .await?;
                    write
                        .write_all(format!("{tag} OK ENABLE completed\r\n").as_bytes())
                        .await?;
                } else {
                    write
                        .write_all(format!("{tag} NO unsupported extension\r\n").as_bytes())
                        .await?;
                }
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
                selected = Some(name.clone());
                let exists = config.folder_messages(&name).len();
                let uidnext = config.ceiling(&name) + 1;
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
                // RFC 7162 §3.1.2: HIGHESTMODSEQ comes back for `SELECT x
                // (CONDSTORE)`, not for a plain SELECT. A client that does not
                // ask must not be handed a modseq it would then checkpoint.
                let asked_for_condstore = command.to_ascii_uppercase().contains("(CONDSTORE)");
                if asked_for_condstore
                    && (config.advertises("CONDSTORE") || config.advertises("QRESYNC"))
                {
                    write
                        .write_all(
                            format!(
                                "* OK [HIGHESTMODSEQ {}] Highest\r\n",
                                config.highest_modseq(&name)
                            )
                            .as_bytes(),
                        )
                        .await?;
                }
                write
                    .write_all(format!("{tag} OK [READ-WRITE] {verb} completed\r\n").as_bytes())
                    .await?;
            }
            "UID" => {
                let mut args = command.splitn(3, ' ').skip(1);
                let sub = args.next().unwrap_or("").to_ascii_uppercase();
                if config.refuse_uid_trycreate {
                    write
                        .write_all(
                            format!("{tag} NO [TRYCREATE] no such destination mailbox\r\n")
                                .as_bytes(),
                        )
                        .await?;
                    continue;
                }
                if config.refuse_uid {
                    write
                        .write_all(format!("{tag} NO [LIMIT] too many requests\r\n").as_bytes())
                        .await?;
                    continue;
                }
                let rest = args.next().unwrap_or("");
                match sub.as_str() {
                    "FETCH" => {
                        let (set, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                        let folder = selected.as_deref().unwrap_or(DEFAULT_FOLDER);
                        serve_uid_fetch(&mut write, &config, folder, set, tail, qresync_enabled)
                            .await?;
                    }
                    "SEARCH" => {
                        let folder = selected.as_deref().unwrap_or(DEFAULT_FOLDER);
                        let uids: Vec<String> = if config.empty_search {
                            Vec::new()
                        } else {
                            config
                                .folder_messages(folder)
                                .iter()
                                .map(|m| m.uid.to_string())
                                .collect()
                        };
                        write
                            .write_all(format!("* SEARCH {}\r\n", uids.join(" ")).as_bytes())
                            .await?;
                    }
                    _ => {}
                }
                write
                    .write_all(format!("{tag} OK UID {sub} completed\r\n").as_bytes())
                    .await?;
            }
            "IDLE" => {
                if !config.advertises("IDLE") {
                    write
                        .write_all(format!("{tag} BAD IDLE not supported\r\n").as_bytes())
                        .await?;
                    continue;
                }
                write.write_all(b"+ idling\r\n").await?;
                idling.fetch_add(1, Ordering::SeqCst);
                // Until DONE arrives, the connection belongs to the server:
                // it may volunteer untagged responses at any moment, and the
                // client may say exactly one thing back.
                let outcome = serve_idle(
                    &mut write,
                    &mut lines,
                    &mut pushes,
                    &shutdown,
                    &command_log,
                    &config,
                    tag,
                )
                .await;
                idling.fetch_sub(1, Ordering::SeqCst);
                match outcome? {
                    IdleExit::Done => {}
                    IdleExit::Closed => return Ok(()),
                }
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

/// How an `IDLE` ended.
enum IdleExit {
    /// The client sent `DONE`; the connection carries on.
    Done,
    /// The connection went away.
    Closed,
}

/// Hold an `IDLE` open, volunteering whatever a test pushes, until `DONE`.
///
/// Every wait here is cancellation safe, so the `select!` cannot lose a line.
#[allow(clippy::too_many_arguments)]
async fn serve_idle<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    lines: &mut tokio::sync::mpsc::Receiver<String>,
    pushes: &mut broadcast::Receiver<String>,
    shutdown: &CancellationToken,
    command_log: &Arc<Mutex<Vec<String>>>,
    config: &MockConfig,
    tag: &str,
) -> std::io::Result<IdleExit> {
    // A real server volunteers `* OK Still here` on a timer so intermediaries
    // do not reap the connection. It is also the response that makes a naive
    // client's re-IDLE cadence never fire, so the mock has to send it.
    let mut keepalive = tokio::time::interval(config.idle_keepalive);
    keepalive.tick().await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(IdleExit::Closed),
            line = lines.recv() => {
                let Some(line) = line else {
                    return Ok(IdleExit::Closed);
                };
                // DONE is untagged, so the main loop never sees it; log it here
                // or a test cannot tell a clean IDLE teardown from an abandoned
                // connection.
                if let Ok(mut log) = command_log.lock() {
                    log.push(line.trim().to_owned());
                }
                if line.trim().eq_ignore_ascii_case("DONE") {
                    write
                        .write_all(format!("{tag} OK IDLE terminated\r\n").as_bytes())
                        .await?;
                    return Ok(IdleExit::Done);
                }
            }
            _ = keepalive.tick(), if config.idle_keepalive < Duration::MAX => {
                write.write_all(b"* OK Still here\r\n").await?;
            }
            pushed = pushes.recv() => {
                match pushed {
                    Ok(line) => {
                        write.write_all(line.as_bytes()).await?;
                        write.write_all(b"\r\n").await?;
                    }
                    // Lagged past the buffer, or the sender is gone: keep
                    // idling either way.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
}

/// Serve the untagged part of a `UID FETCH`, honoring `CHANGEDSINCE` and
/// `VANISHED`.
async fn serve_uid_fetch<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    config: &MockConfig,
    selected: &str,
    set: &str,
    tail: &str,
    qresync_enabled: bool,
) -> std::io::Result<()> {
    let modifiers = parse_fetch_modifiers(tail);
    let ceiling = config.ceiling(selected);

    // VANISHED (EARLIER) precedes the fetch data, as a real server sends it.
    if modifiers.vanished {
        if let Some(since) = modifiers.changedsince {
            let requested = uids_in_set(set, ceiling);
            let mut gone: Vec<u32> = config
                .folder_expunged(selected)
                .iter()
                .filter(|(uid, modseq)| *modseq > since && requested.contains(uid))
                .map(|(uid, _)| *uid)
                .collect();
            gone.sort_unstable();
            if !gone.is_empty() {
                write
                    .write_all(
                        format!("* VANISHED (EARLIER) {}\r\n", render_uid_set(&gone)).as_bytes(),
                    )
                    .await?;
            }
        }
    }

    let messages = config.folder_messages(selected);
    let wanted = uids_in_set(set, config.max_uid(selected));
    for (seq, spec) in messages.iter().enumerate() {
        if !wanted.contains(&spec.uid) {
            continue;
        }
        if modifiers
            .changedsince
            .is_some_and(|since| spec.modseq <= since)
        {
            continue;
        }
        let flags = spec.flags.join(" ");
        let mut head = format!(
            "* {} FETCH (UID {} FLAGS ({flags}) MODSEQ ({})",
            seq + 1,
            spec.uid,
            spec.modseq
        );
        if modifiers.wants_body {
            let n = spec.raw.len();
            head.push_str(&format!(
                " INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" \
                 RFC822.SIZE {n} BODY[] {{{n}}}\r\n"
            ));
            write.write_all(head.as_bytes()).await?;
            write.write_all(&spec.raw).await?;
            write.write_all(b")\r\n").await?;
        } else {
            head.push_str(")\r\n");
            write.write_all(head.as_bytes()).await?;
        }
    }

    // RFC 7162 §3.2.10: once QRESYNC is enabled, the server reports expunges as
    // VANISHED rather than EXPUNGE — unsolicited, at whatever moment it likes,
    // including on the back of a command that asked for nothing of the sort.
    // Modelling this is what makes the session-scoped nature of those responses
    // visible: a client that leaves one sitting in the channel will read it
    // back while a different mailbox is selected.
    if qresync_enabled && !modifiers.vanished {
        let mut gone: Vec<u32> = config
            .folder_expunged(selected)
            .iter()
            .map(|(uid, _)| *uid)
            .collect();
        gone.sort_unstable();
        if !gone.is_empty() {
            write
                .write_all(format!("* VANISHED {}\r\n", render_uid_set(&gone)).as_bytes())
                .await?;
        }
    }
    Ok(())
}
