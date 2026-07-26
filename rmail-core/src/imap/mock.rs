//! A minimal in-process mock IMAP server for tests (no network, plaintext).
//!
//! It speaks just enough of the protocol to exercise login, capability probing,
//! folder listing, and logout: it echoes the command tag, accepts/rejects
//! `LOGIN` against a configured password, and returns a fixed capability set and
//! folder list.

use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Configuration for a mock server run.
#[derive(Clone)]
pub(crate) struct MockConfig {
    password: String,
    /// `(name, attributes)` pairs returned by `LIST`.
    folders: Vec<(String, String)>,
    /// A single message returned by `UID FETCH`, if configured.
    fetch: Option<FetchSpec>,
}

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
            fetch: None,
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

    /// Set the single message returned by `UID FETCH`.
    pub(crate) fn fetch(mut self, uid: u32, flags: &[&str], raw: &[u8]) -> Self {
        self.fetch = Some(FetchSpec {
            uid,
            flags: flags.iter().map(|f| (*f).to_owned()).collect(),
            raw: raw.to_vec(),
        });
        self
    }
}

/// A running mock server bound to an ephemeral loopback port.
pub(crate) struct MockImap {
    /// Address to connect to.
    pub(crate) addr: SocketAddr,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockImap {
    /// Bind and start serving one connection.
    pub(crate) async fn start(config: MockConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                let _ = serve(sock, config).await;
            }
        });
        Self {
            addr,
            _handle: handle,
        }
    }
}

async fn serve(sock: TcpStream, config: MockConfig) -> std::io::Result<()> {
    let (read_half, mut write) = sock.into_split();
    let mut reader = BufReader::new(read_half);

    write
        .write_all(b"* OK [CAPABILITY IMAP4rev1] rmail mock ready\r\n")
        .await?;

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
            "UID" => {
                // Only `UID FETCH` is modeled.
                if let Some(spec) = &config.fetch {
                    let raw = &spec.raw;
                    let n = raw.len();
                    let flags = spec.flags.join(" ");
                    let head = format!(
                        "* 1 FETCH (UID {} FLAGS ({flags}) \
                         INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" \
                         RFC822.SIZE {n} BODY[] {{{n}}}\r\n",
                        spec.uid
                    );
                    write.write_all(head.as_bytes()).await?;
                    write.write_all(raw).await?;
                    write.write_all(b")\r\n").await?;
                }
                write
                    .write_all(format!("{tag} OK UID FETCH completed\r\n").as_bytes())
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
