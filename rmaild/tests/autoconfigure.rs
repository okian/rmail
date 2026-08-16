//! Integration test: `AccountService.Autoconfigure` end-to-end against an
//! in-process tonic server over a Unix domain socket (task 80).
//!
//! `rmail_core::autoconfig`'s own suite proves the probes, the validator and
//! the login check. What is only observable from here is the *boundary*: the
//! `Status` codes a client branches on, the projection of a domain
//! [`Proposal`] onto the wire (an absent SMTP server, an
//! `Option<i64>` account id becoming `0`), and that the whole thing is
//! reachable through the daemon's real boot wiring rather than a
//! hand-assembled handler.
//!
//! No test here touches the network. The daemon's probe endpoints are the one
//! injected dependency (`rmaild::Injected::autoconfig_endpoints`, the same
//! seam the AI provider already uses), pointed at a loopback HTTP server this
//! file controls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rmail_core::autoconfig::ProbeEndpoints;
use rmail_core::events::{EventLog, Retention};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::{credential_ref, AutoconfigureRequest, CredentialRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// A loopback HTTP server the daemon's probes talk to
// ---------------------------------------------------------------------------

struct Http {
    base: String,
    task: JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Http {
    async fn start(routes: Vec<(&'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table = Arc::new(Mutex::new(routes));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let table = Arc::clone(&table);
                tokio::spawn(serve_one(stream, table));
            }
        });
        Self {
            base: format!("http://{addr}"),
            task,
        }
    }

    fn endpoints(&self) -> ProbeEndpoints {
        ProbeEndpoints {
            ispdb_base: format!("{}/ispdb", self.base),
            domain_base: Some(self.base.clone()),
            doh_endpoint: format!("{}/dns-query", self.base),
        }
    }
}

async fn serve_one(mut stream: TcpStream, table: Arc<Mutex<Vec<(&'static str, String)>>>) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let Ok(read) = stream.read(&mut buf).await else {
            return;
        };
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&raw).to_string();
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let found = table
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(prefix, _)| target.starts_with(prefix))
        .map(|(_, body)| body.clone());
    let (status, body) = found.map_or((404, String::new()), |body| (200, body));
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn mozilla_doc(imap: &str, smtp: Option<&str>) -> String {
    let outgoing = smtp.map_or(String::new(), |smtp| {
        format!(
            "<outgoingServer type=\"smtp\"><hostname>{smtp}</hostname><port>587</port>\
             <socketType>STARTTLS</socketType></outgoingServer>"
        )
    });
    format!(
        r#"<?xml version="1.0"?>
<clientConfig version="1.1"><emailProvider id="example.com">
  <incomingServer type="imap">
    <hostname>{imap}</hostname><port>993</port><socketType>SSL</socketType>
    <username>%EMAILADDRESS%</username>
  </incomingServer>
  {outgoing}
</emailProvider></clientConfig>"#
    )
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// A daemon whose autoconfig probes point at `endpoints`. `None` leaves
    /// the real endpoints in place — used only by the test that never lets a
    /// probe run.
    async fn start(endpoints: Option<ProbeEndpoints>) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-autoconf-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-autoconf-rpc-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), Retention::unlimited());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            // No provider for this suite. `ai.enabled` defaults *on*, and
            // building a Claude client does not validate its key, so a daemon
            // left at the default would construct an inferrer and fail the
            // model fallback at key resolution (`UNAUTHENTICATED`) instead of
            // at the precondition the caller actually tripped. None of these
            // tests script a provider; autoconfig discovery is the subject.
            config.ai.enabled = false;
            rmaild::serve_uds_injected(
                &server_socket,
                server_db.clone(),
                engine,
                rmail_core::mail::MailStore::new(
                    server_db.clone(),
                    log.clone(),
                    Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                        server_db.clone(),
                    )),
                ),
                rmail_core::tags::TagStore::new(
                    server_db.clone(),
                    Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(server_db)),
                    config.tags.clone(),
                ),
                &config,
                rmaild::Injected {
                    autoconfig_endpoints: endpoints,
                    ..Default::default()
                },
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
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AccountServiceClient<Channel> {
        AccountServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
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

fn ask(email: &str) -> AutoconfigureRequest {
    AutoconfigureRequest {
        email: email.to_owned(),
        credential: None,
        allow_model_fallback: false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_discovery_comes_back_as_settings_and_a_pasteable_block() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        mozilla_doc("imap.example.com", Some("smtp.example.com")),
    )])
    .await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let response = server
        .client()
        .await
        .autoconfigure(ask("ada@example.com"))
        .await
        .expect("Autoconfigure")
        .into_inner();

    assert_eq!(response.source, "autoconfig");
    let imap = response.imap.expect("an incoming server");
    assert_eq!(imap.host, "imap.example.com");
    assert_eq!(imap.port, 993);
    assert_eq!(imap.security, "tls");
    assert_eq!(imap.username, "ada@example.com");
    assert_eq!(response.smtp.expect("an outgoing server").port, 587);
    // No account exists for this address, and the projection of `None` is 0.
    assert_eq!(response.existing_account_id, 0);
    // No credential was supplied, so the response says it was not verified
    // rather than leaving `login_validated = false` to be read as a failure.
    assert!(!response.login_validated);
    assert!(
        response.validation_detail.contains("no credential"),
        "detail: {}",
        response.validation_detail
    );
    assert!(
        response.toml.contains("[[accounts]]") && response.toml.contains("imap.example.com"),
        "toml: {}",
        response.toml
    );

    // And nothing was written: `Autoconfigure` proposes.
    let accounts = rmail_core::account::list(&server.db).await.unwrap();
    assert!(accounts.is_empty(), "the RPC created an account");
    server.stop().await;
}

#[tokio::test]
async fn a_document_with_no_outgoing_server_leaves_smtp_absent_and_says_so() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        mozilla_doc("imap.example.com", None),
    )])
    .await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let response = server
        .client()
        .await
        .autoconfigure(ask("ada@example.com"))
        .await
        .expect("Autoconfigure")
        .into_inner();

    assert!(response.smtp.is_none());
    assert!(
        response.warnings.iter().any(|w| w.contains("SMTP")),
        "warnings: {:?}",
        response.warnings
    );
    server.stop().await;
}

#[tokio::test]
async fn an_existing_account_comes_back_by_id_and_is_left_alone() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        mozilla_doc("imap.example.com", Some("smtp.example.com")),
    )])
    .await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let existing = rmail_core::account::create(
        &server.db,
        rmail_core::account::NewAccount {
            name: "ada@example.com".to_owned(),
            imap_server: Some("legacy.example.com".to_owned()),
            imap_port: Some(143),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let response = server
        .client()
        .await
        .autoconfigure(ask("ada@example.com"))
        .await
        .expect("Autoconfigure")
        .into_inner();

    assert_eq!(response.existing_account_id, existing.id);
    assert!(
        response
            .warnings
            .iter()
            .any(|w| w.contains("legacy.example.com")),
        "the difference must be spelled out: {:?}",
        response.warnings
    );
    let after = rmail_core::account::get(&server.db, existing.id)
        .await
        .unwrap();
    assert_eq!(after.imap_server.as_deref(), Some("legacy.example.com"));
    assert_eq!(after.imap_port, Some(143));
    server.stop().await;
}

#[tokio::test]
async fn a_malformed_address_is_invalid_argument_before_any_probe_runs() {
    // No endpoints injected: reaching a probe here would mean reaching the
    // real network, so this test failing is also how that regression shows up.
    let server = TestServer::start(None).await;
    let mut client = server.client().await;
    for bad in ["", "no-at-sign", "ada@", "ada@localhost", "ada@127.0.0.1"] {
        let status = client
            .autoconfigure(ask(bad))
            .await
            .expect_err(&format!("{bad:?} must be refused"));
        assert_eq!(status.code(), Code::InvalidArgument, "{bad:?}");
    }
    server.stop().await;
}

#[tokio::test]
async fn a_hostile_document_is_a_failed_precondition_not_a_bad_argument() {
    // The caller's argument was fine; the domain's document was not. Telling
    // a client to fix input that was already correct sends it round a loop it
    // cannot escape.
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        mozilla_doc("imap.example.com", Some("smtp.example.com")).replace(
            "<socketType>SSL</socketType>",
            "<socketType>plain</socketType>",
        ),
    )])
    .await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let status = server
        .client()
        .await
        .autoconfigure(ask("ada@example.com"))
        .await
        .expect_err("a plaintext server must not be configured");
    assert_eq!(status.code(), Code::FailedPrecondition);
    server.stop().await;
}

#[tokio::test]
async fn nothing_published_is_not_found_and_names_the_fallback() {
    let http = Http::start(vec![]).await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let status = server
        .client()
        .await
        .autoconfigure(ask("ada@example.com"))
        .await
        .expect_err("nothing was published");
    assert_eq!(status.code(), Code::NotFound);
    assert!(
        status.message().contains("model fallback"),
        "message: {}",
        status.message()
    );
    server.stop().await;
}

#[tokio::test]
async fn the_model_fallback_is_declined_on_a_daemon_with_no_provider() {
    // This suite's daemon runs with `ai.enabled = false` (see `TestServer`),
    // so it genuinely has no inferrer — and says so, rather than answering
    // "nothing found" to a caller who explicitly asked for a guess.
    //
    // The distinction matters: `ai.enabled` defaults *on*, so a daemon left
    // at the default builds a provider whose key only fails when it is used,
    // and the caller would get `UNAUTHENTICATED` — an accurate description of
    // that daemon, but not of this one.
    let http = Http::start(vec![]).await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let status = server
        .client()
        .await
        .autoconfigure(AutoconfigureRequest {
            email: "ada@example.com".to_owned(),
            credential: None,
            allow_model_fallback: true,
        })
        .await
        .expect_err("no provider is wired");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("AI provider"),
        "message: {}",
        status.message()
    );
    server.stop().await;
}

#[tokio::test]
async fn a_credential_reference_travels_but_the_secret_does_not_come_back() {
    // The login itself cannot succeed here — `imap.example.com` is not
    // reachable from a test, and it must not be: nothing in this suite may
    // dial anything but loopback. What is asserted is the contract around it:
    // a `CredentialRef` is accepted, the failure is *reported* rather than
    // raised as a `Status`, and nothing in the response echoes the reference.
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        mozilla_doc("imap.invalid", Some("smtp.invalid")),
    )])
    .await;
    let server = TestServer::start(Some(http.endpoints())).await;
    let response = server
        .client()
        .await
        .autoconfigure(AutoconfigureRequest {
            email: "ada@example.com".to_owned(),
            credential: Some(CredentialRef {
                source: Some(credential_ref::Source::PasswordEnv(
                    "RMAIL_TEST_NO_SUCH_VAR".to_owned(),
                )),
            }),
            allow_model_fallback: false,
        })
        .await
        .expect("a proposal even when the login cannot be made")
        .into_inner();

    assert_eq!(response.imap.expect("imap").host, "imap.invalid");
    assert!(!response.login_validated);
    assert!(
        !response.validation_detail.is_empty(),
        "an unverified proposal must say why"
    );
    assert!(
        !response.toml.contains("RMAIL_TEST_NO_SUCH_VAR") || response.toml.contains("password_env"),
        "a credential reference may appear only as the config key it is"
    );
    server.stop().await;
}
