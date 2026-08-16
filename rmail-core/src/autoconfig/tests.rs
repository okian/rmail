//! What task 80's autoconfig half owes, proved rather than asserted:
//!
//! - each probe, driven end to end against a **local** HTTP server this suite
//!   controls — no test here touches the real network, the same discipline
//!   `ai::provider` and `embed::voyage` follow;
//! - every discovered value going through the validator, whatever produced
//!   it: a document, a DNS answer, or a model;
//! - the refusals that matter — an IP literal, a plaintext socket type, an
//!   out-of-range port, an oversized body — each one a *refusal*, not a
//!   fallback to a plausible default;
//! - login validation against a real IMAP server (the in-crate mock),
//!   including a rejected login being reported rather than raised;
//! - the model fallback being fenced, opt-in, unattributed-but-budgeted, and
//!   proposing rather than committing;
//! - an already-configured account being reported and never touched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatRequest, ChatResponse, Provider, ProviderStream, StopReason, Usage};
use crate::config::{AiLimits, Config};
use crate::credential::Secret;
use crate::imap::mock::{MockConfig, MockImap};
use crate::storage::Database;
use crate::ErrorReason;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// A loopback HTTP server that answers by path
// ---------------------------------------------------------------------------

/// An HTTP server answering a fixed routing table, recording what it was
/// asked. A real socket rather than a mocked client, so the request under test
/// is the one `reqwest` would actually send.
struct Http {
    base: String,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        // A `JoinHandle` does not abort on drop; without this every test
        // leaves an accept loop and a bound port behind.
        self.task.abort();
    }
}

impl Http {
    /// `routes` maps a path prefix to `(status, body)`. Anything unmatched is
    /// a 404 — which is exactly what a domain that publishes nothing serves.
    async fn start(routes: Vec<(&'static str, u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let table: Arc<Vec<(&'static str, u16, String)>> = Arc::new(routes);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let table = Arc::clone(&table);
                tokio::spawn(serve_one(stream, recorder, table));
            }
        });
        Self {
            base: format!("http://{addr}"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Endpoints pointing every probe at this server.
    fn endpoints(&self) -> ProbeEndpoints {
        ProbeEndpoints {
            ispdb_base: format!("{}/ispdb", self.base),
            domain_base: Some(self.base.clone()),
            doh_endpoint: format!("{}/dns-query", self.base),
        }
    }
}

async fn serve_one(
    mut stream: TcpStream,
    recorder: Arc<Mutex<Vec<String>>>,
    table: Arc<Vec<(&'static str, u16, String)>>,
) {
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
    recorder
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(target.clone());

    let (status, body) = table
        .iter()
        .find(|(prefix, _, _)| target.starts_with(prefix))
        .map_or((404, String::new()), |(_, status, body)| {
            (*status, body.clone())
        });
    // Two statuses mean something other than "here is a body":
    //   3xx — the body is the `Location`, so a test can offer a redirect.
    //   599 — never answer at all, so a test can hang a probe on purpose.
    if status == HANG_STATUS {
        std::future::pending::<()>().await;
        return;
    }
    let response = if (300..400).contains(&status) {
        format!(
            "HTTP/1.1 {status} X\r\nLocation: {body}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
    } else {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// The pseudo-status that makes the mock accept a connection and never answer.
const HANG_STATUS: u16 = 599;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn open_db() -> (Database, PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rmail-autoconf-{pid}-{n}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    let db = Database::open(&path).expect("open test db");
    (db, path)
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
}

/// A login probe that never connects — it just records and answers.
#[derive(Debug, Default)]
struct StubLogin {
    seen: Mutex<Vec<(String, u16, String)>>,
    reject: bool,
}

impl StubLogin {
    fn attempts(&self) -> Vec<(String, u16, String)> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LoginProbe for StubLogin {
    async fn login(
        &self,
        settings: &ServerSettings,
        username: &str,
        _secret: &Secret,
    ) -> Result<(), Error> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((settings.host.clone(), settings.port, username.to_owned()));
        if self.reject {
            return Err(Error::unauthenticated("IMAP login rejected: bad password"));
        }
        Ok(())
    }
}

/// A login probe that speaks real IMAP to the in-crate mock server over a
/// plaintext socket — the same `async-imap` login path production uses, minus
/// the TLS wrapper the mock does not speak.
#[derive(Debug)]
struct MockLogin {
    addr: std::net::SocketAddr,
}

#[async_trait]
impl LoginProbe for MockLogin {
    async fn login(
        &self,
        _settings: &ServerSettings,
        username: &str,
        secret: &Secret,
    ) -> Result<(), Error> {
        let stream = TcpStream::connect(self.addr)
            .await
            .map_err(|e| Error::unavailable(format!("cannot reach the mock: {e}")))?;
        let mut session = crate::imap::conn::login(stream, username, secret.expose()).await?;
        let _ = session.logout().await;
        Ok(())
    }
}

/// A provider that answers from a queue and records every request.
#[derive(Debug, Default)]
struct MockProvider {
    answers: Mutex<Vec<String>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    fn answering(answer: &str) -> Arc<Self> {
        let provider = Arc::new(Self::default());
        provider
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(answer.to_owned());
        provider
    }

    fn calls(&self) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Every character that would have left the host.
    fn transmitted(&self) -> String {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = String::new();
        for request in seen.iter() {
            out.push_str(request.system.as_deref().unwrap_or_default());
            for message in &request.messages {
                out.push_str(&message.content);
            }
        }
        out
    }

    fn last_system(&self) -> String {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .and_then(|r| r.system.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let text = self
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default();
        Ok(ChatResponse {
            id: "msg_test".to_owned(),
            model: request.model.clone(),
            stop_reason: StopReason::EndTurn,
            text,
            usage: Usage::default(),
        })
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::unavailable("autoconfig never streams"))
    }
}

fn inferrer(
    db: &Database,
    provider: Arc<MockProvider>,
    config: &Config,
) -> infer::SettingsInferrer {
    infer::SettingsInferrer::new(
        db.clone(),
        provider as Arc<dyn Provider>,
        Arc::new(PolicyEngine::from_config(config).expect("policy engine")),
        AiLimits::default(),
        Arc::new(tokio::sync::Semaphore::new(4)),
        Arc::new(crate::ai::queue::RateLimiter::new(600)),
        "claude-haiku-4-5",
    )
}

fn request(email: &str) -> AutoconfigRequest {
    AutoconfigRequest {
        email: email.to_owned(),
        credential: CredentialSource::None,
        allow_model_fallback: false,
    }
}

/// A Mozilla autoconfig document for `domain`.
fn mozilla_doc(host: &str, socket: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <incomingServer type="pop3">
      <hostname>pop.{host}</hostname><port>995</port><socketType>SSL</socketType>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>imap.{host}</hostname>
      <port>993</port>
      <socketType>{socket}</socketType>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.{host}</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILLOCALPART%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#
    )
}

fn autodiscover_doc(host: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a">
    <Account>
      <Protocol>
        <Type>IMAP</Type>
        <Server>imap.{host}</Server>
        <Port>993</Port>
        <SSL>on</SSL>
        <LoginName>%EMAILADDRESS%</LoginName>
      </Protocol>
      <Protocol>
        <Type>SMTP</Type>
        <Server>smtp.{host}</Server>
        <Port>465</Port>
        <SSL>on</SSL>
      </Protocol>
    </Account>
  </Response>
</Autodiscover>"#
    )
}

fn srv_answer(port: u16, target: &str) -> String {
    format!(
        r#"{{"Status":0,"Answer":[{{"name":"_imaps._tcp.example.com","type":33,
        "data":"10 5 {port} {target}."}}]}}"#
    )
}

// ---------------------------------------------------------------------------
// The validator
// ---------------------------------------------------------------------------

#[test]
fn a_discovered_ip_literal_is_refused() {
    // The threat this exists for: a document served by the domain being
    // configured aiming a login — with the user's password — at an address
    // inside the network this daemon runs in.
    for literal in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "0.0.0.0"] {
        let error = validate::host(literal).expect_err(literal);
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{literal}");
        assert!(
            error.to_string().contains("IP literal"),
            "the refusal should say why: {error}"
        );
    }
}

#[test]
fn a_discovered_hostname_must_be_a_fully_qualified_ascii_name() {
    for bad in [
        "",
        "   ",
        "localhost",
        "imap",
        "imap..example.com",
        "-imap.example.com",
        "imap-.example.com",
        "imap.example.com:993",
        "imap.example.com/path",
        "imap.example.com\nhost: evil",
        "imap.exämple.com",
        "user@imap.example.com",
        "imap.example.1",
        "imap.example.com\"",
    ] {
        let error = validate::host(bad).expect_err(&format!("{bad:?} should be refused"));
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{bad:?}");
    }
    // 254 bytes of otherwise-valid name.
    let long = format!("{}.example.com", "a".repeat(250));
    assert!(validate::host(&long).is_err());
}

#[test]
fn a_valid_hostname_is_normalized() {
    assert_eq!(
        validate::host("  IMAP.Example.COM.  ").expect("valid"),
        "imap.example.com"
    );
    assert_eq!(
        validate::host("xn--80ak6aa92e.com").expect("punycode is a valid name"),
        "xn--80ak6aa92e.com"
    );
}

#[test]
fn a_plaintext_socket_type_is_refused_rather_than_downgraded() {
    for plaintext in ["plain", "PLAIN", "none", "off", "cleartext"] {
        let error = Security::parse(plaintext).expect_err(plaintext);
        assert_eq!(
            error.reason(),
            ErrorReason::FailedPrecondition,
            "{plaintext} should be a refusal"
        );
    }
    // An unrecognized value is not evidence of encryption either.
    assert_eq!(
        Security::parse("magic").expect_err("unknown").reason(),
        ErrorReason::InvalidArgument
    );
    assert_eq!(Security::parse("SSL").expect("ssl"), Security::Tls);
    assert_eq!(Security::parse("on").expect("on"), Security::Tls);
    assert_eq!(
        Security::parse("STARTTLS").expect("starttls"),
        Security::StartTls
    );
    assert!(Security::StartTls.is_weaker_than(Security::Tls));
    assert!(!Security::Tls.is_weaker_than(Security::StartTls));
}

#[test]
fn a_port_must_be_in_range() {
    assert_eq!(validate::port(993).expect("993"), 993);
    assert_eq!(validate::port(65535).expect("65535"), 65535);
    for bad in [0, -1, 65536, 1 << 40] {
        assert_eq!(
            validate::port(bad).expect_err(&bad.to_string()).reason(),
            ErrorReason::InvalidArgument
        );
    }
}

#[test]
fn an_address_needs_exactly_one_at_and_a_valid_domain() {
    for bad in [
        "no-at-sign",
        "@example.com",
        "ada@",
        "a@b@example.com",
        "ada@localhost",
        "ada@127.0.0.1",
        "a da@example.com",
        "ada\n@example.com",
    ] {
        assert_eq!(
            Address::parse(bad).expect_err(bad).reason(),
            ErrorReason::InvalidArgument,
            "{bad:?}"
        );
    }
    let parsed = Address::parse(" Ada@Example.COM ").expect("valid");
    assert_eq!(parsed.local, "Ada");
    assert_eq!(parsed.domain, "example.com");
    assert_eq!(parsed.email, "Ada@example.com");
}

// ---------------------------------------------------------------------------
// Document and DNS parsing
// ---------------------------------------------------------------------------

#[test]
fn a_mozilla_document_yields_the_imap_and_smtp_servers() {
    let candidate =
        probe::parse_mozilla_autoconfig(&mozilla_doc("example.com", "SSL")).expect("a candidate");
    assert_eq!(candidate.imap.host, "imap.example.com");
    assert_eq!(candidate.imap.port, "993");
    assert_eq!(candidate.imap.security, "SSL");
    // The POP3 block must not be mistaken for the IMAP one.
    let smtp = candidate.smtp.expect("an outgoing server");
    assert_eq!(smtp.host, "smtp.example.com");
    assert_eq!(smtp.security, "STARTTLS");
}

#[test]
fn a_document_with_no_imap_server_yields_nothing() {
    let pop_only = r#"<clientConfig><emailProvider>
        <incomingServer type="pop3"><hostname>pop.example.com</hostname>
        <port>995</port><socketType>SSL</socketType></incomingServer>
        </emailProvider></clientConfig>"#;
    assert!(probe::parse_mozilla_autoconfig(pop_only).is_none());
    // Malformed XML is a miss, not a panic.
    assert!(probe::parse_mozilla_autoconfig("<clientConfig><incoming").is_none());
    assert!(probe::parse_mozilla_autoconfig("").is_none());
}

#[test]
fn an_autodiscover_document_yields_the_imap_and_smtp_servers() {
    let candidate = probe::parse_autodiscover(&autodiscover_doc("example.com")).expect("candidate");
    assert_eq!(candidate.imap.host, "imap.example.com");
    assert_eq!(candidate.imap.port, "993");
    assert_eq!(candidate.imap.security, "on");
    assert_eq!(candidate.smtp.expect("smtp").port, "465");
}

#[test]
fn an_srv_answer_picks_the_best_record_deterministically() {
    let body = r#"{"Status":0,"Answer":[
        {"name":"_imaps._tcp.example.com","type":33,"data":"20 1 993 b.example.com."},
        {"name":"_imaps._tcp.example.com","type":33,"data":"10 1 993 z.example.com."},
        {"name":"_imaps._tcp.example.com","type":33,"data":"10 9 993 m.example.com."},
        {"name":"_imaps._tcp.example.com","type":5,"data":"ignored.example.com."}
    ]}"#;
    // Lowest priority, then highest weight.
    assert_eq!(
        probe::parse_srv(body),
        Some(("m.example.com.".to_owned(), 993))
    );
    // RFC 2782's "decidedly not available".
    let none = r#"{"Status":0,"Answer":[{"type":33,"data":"0 0 0 ."}]}"#;
    assert_eq!(probe::parse_srv(none), None);
    assert_eq!(probe::parse_srv("not json"), None);
    assert_eq!(probe::parse_srv(r#"{"Status":3}"#), None);
}

#[test]
fn mx_records_come_back_best_first_and_bounded() {
    let body = r#"{"Answer":[
        {"type":15,"data":"20 backup.example.net."},
        {"type":15,"data":"10 primary.example.net."},
        {"type":1,"data":"1.2.3.4"}
    ]}"#;
    assert_eq!(
        probe::parse_mx(body),
        vec!["primary.example.net.", "backup.example.net."]
    );
    let many: Vec<String> = (0..40)
        .map(|i| format!(r#"{{"type":15,"data":"{i} mx{i}.example.net."}}"#))
        .collect();
    let flood = format!(r#"{{"Answer":[{}]}}"#, many.join(","));
    assert!(
        probe::parse_mx(&flood).len() <= 8,
        "a model prompt is not a place to paste an unbounded list"
    );
}

// ---------------------------------------------------------------------------
// Discovery, end to end against a local server
// ---------------------------------------------------------------------------

async fn discover_with(
    routes: Vec<(&'static str, u16, String)>,
    req: AutoconfigRequest,
) -> (Result<Proposal, Error>, Http, Database, PathBuf) {
    let http = Http::start(routes).await;
    let (db, path) = open_db().await;
    let probes = Arc::new(Probes::new(http.endpoints()).expect("probes"));
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        probes,
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let result = engine.discover(&req, &CancellationToken::new()).await;
    (result, http, db, path)
}

#[tokio::test]
async fn the_domains_own_document_is_preferred_and_answers_first() {
    let (result, http, _db, path) = discover_with(
        vec![
            (
                "/mail/config-v1.1.xml",
                200,
                mozilla_doc("example.com", "SSL"),
            ),
            ("/ispdb", 200, mozilla_doc("ispdb.example.com", "SSL")),
        ],
        request("ada@example.com"),
    )
    .await;
    let proposal = result.expect("a proposal");
    assert_eq!(proposal.source, Source::Autoconfig);
    assert_eq!(proposal.imap.host, "imap.example.com");
    assert_eq!(proposal.imap.port, 993);
    assert_eq!(proposal.imap.security, Security::Tls);
    assert_eq!(proposal.imap.username, "ada@example.com");
    let smtp = proposal.smtp.expect("smtp");
    assert_eq!(smtp.host, "smtp.example.com");
    assert_eq!(smtp.port, 587);
    // `%EMAILLOCALPART%` expands to the local part, not the whole address.
    assert_eq!(smtp.username, "ada");
    // The ISPDB was never asked: the domain's own statement outranks it.
    assert!(
        !http.requests().iter().any(|r| r.starts_with("/ispdb")),
        "requests: {:?}",
        http.requests()
    );
    cleanup(&path);
}

#[tokio::test]
async fn the_ispdb_answers_when_the_domain_publishes_nothing() {
    let (result, http, _db, path) = discover_with(
        vec![("/ispdb", 200, mozilla_doc("ispdb.example.com", "SSL"))],
        request("ada@example.com"),
    )
    .await;
    let proposal = result.expect("a proposal");
    assert_eq!(proposal.source, Source::Ispdb);
    assert_eq!(proposal.imap.host, "imap.ispdb.example.com");
    // The ISPDB is looked up by domain.
    assert!(
        http.requests().iter().any(|r| r == "/ispdb/example.com"),
        "requests: {:?}",
        http.requests()
    );
    cleanup(&path);
}

#[tokio::test]
async fn autodiscover_answers_when_the_first_two_miss() {
    let (result, _http, _db, path) = discover_with(
        vec![(
            "/autodiscover/autodiscover.xml",
            200,
            autodiscover_doc("outlook.example.com"),
        )],
        request("ada@example.com"),
    )
    .await;
    let proposal = result.expect("a proposal");
    assert_eq!(proposal.source, Source::Autodiscover);
    assert_eq!(proposal.imap.host, "imap.outlook.example.com");
    assert_eq!(proposal.imap.security, Security::Tls);
    cleanup(&path);
}

#[tokio::test]
async fn an_srv_record_answers_when_every_document_misses() {
    let (result, http, _db, path) = discover_with(
        vec![("/dns-query", 200, srv_answer(993, "imap.srv.example.com"))],
        request("ada@example.com"),
    )
    .await;
    let proposal = result.expect("a proposal");
    assert_eq!(proposal.source, Source::Srv);
    // The trailing dot of an SRV target is stripped by the validator.
    assert_eq!(proposal.imap.host, "imap.srv.example.com");
    assert_eq!(proposal.imap.port, 993);
    // RFC 6186 names the service, never the user: the address is the fallback.
    assert_eq!(proposal.imap.username, "ada@example.com");
    assert!(
        http.requests()
            .iter()
            .any(|r| r.contains("_imaps._tcp.example.com")),
        "requests: {:?}",
        http.requests()
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_document_offering_only_plaintext_is_refused() {
    let (result, _http, _db, path) = discover_with(
        vec![(
            "/mail/config-v1.1.xml",
            200,
            mozilla_doc("example.com", "plain"),
        )],
        request("ada@example.com"),
    )
    .await;
    let error = result.expect_err("a plaintext server must not be configured");
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);
    assert!(
        error.to_string().contains("unencrypted"),
        "the refusal should say why: {error}"
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_document_naming_an_internal_address_is_refused() {
    let (result, _http, _db, path) = discover_with(
        vec![(
            "/mail/config-v1.1.xml",
            200,
            mozilla_doc("example.com", "SSL").replace("imap.example.com", "169.254.169.254"),
        )],
        request("ada@example.com"),
    )
    .await;
    let error = result.expect_err("an IP literal must not be configured");
    // FAILED_PRECONDITION, not INVALID_ARGUMENT: the caller's argument was
    // fine, a third party's document was not, and telling a client to fix
    // input that was already correct sends it round a loop it cannot escape.
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);
    cleanup(&path);
}

#[tokio::test]
async fn a_document_naming_an_out_of_range_port_is_refused() {
    let (result, _http, _db, path) = discover_with(
        vec![(
            "/mail/config-v1.1.xml",
            200,
            mozilla_doc("example.com", "SSL").replace("<port>993</port>", "<port>0</port>"),
        )],
        request("ada@example.com"),
    )
    .await;
    assert_eq!(
        result.expect_err("port 0").reason(),
        ErrorReason::FailedPrecondition
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_response_body_is_read_only_to_its_cap() {
    // The settings sit past the cap, behind a comment a hostile server could
    // stream forever. Reading to the cap means they are never seen — which is
    // the point: the bound holds even when honoring it costs the answer.
    let padding = "<!--".to_owned() + &"p".repeat(probe::MAX_BODY_BYTES) + "-->";
    let document = format!("{padding}{}", mozilla_doc("example.com", "SSL"));
    let (result, _http, _db, path) = discover_with(
        vec![("/mail/config-v1.1.xml", 200, document)],
        request("ada@example.com"),
    )
    .await;
    let error = result.expect_err("the truncated document must not parse");
    assert_eq!(error.reason(), ErrorReason::NotFound);
    cleanup(&path);
}

#[tokio::test]
async fn nothing_discovered_and_no_fallback_asked_for_is_not_found() {
    let (result, _http, _db, path) = discover_with(vec![], request("ada@example.com")).await;
    let error = result.expect_err("nothing was published");
    assert_eq!(error.reason(), ErrorReason::NotFound);
    assert!(error.to_string().contains("model fallback"));
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// Login validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_discovery_is_verified_by_a_real_imap_login() {
    let mock = MockImap::start(MockConfig::default().password("hunter2")).await;
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(MockLogin { addr: mock.addr }) as Arc<dyn LoginProbe>,
    );

    let proposal = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential: CredentialSource::Command("printf hunter2".to_owned()),
                allow_model_fallback: false,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a proposal");

    assert!(proposal.login_validated, "{}", proposal.validation_detail);
    assert!(proposal.validation_detail.is_empty());
    // A real login, on the wire.
    assert!(
        mock.commands().iter().any(|c| c.starts_with("LOGIN")),
        "commands: {:?}",
        mock.commands()
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_rejected_login_is_reported_not_raised() {
    // The settings are still the best answer available; hiding them behind an
    // UNAUTHENTICATED would leave the user with nothing to fix.
    let mock = MockImap::start(MockConfig::default().password("hunter2")).await;
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(MockLogin { addr: mock.addr }) as Arc<dyn LoginProbe>,
    );

    let proposal = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential: CredentialSource::Command("printf wrong".to_owned()),
                allow_model_fallback: false,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a proposal even on a refused login");

    assert!(!proposal.login_validated);
    assert!(
        proposal.validation_detail.contains("login failed"),
        "detail: {}",
        proposal.validation_detail
    );
    assert_eq!(proposal.imap.host, "imap.example.com");
    cleanup(&path);
}

#[tokio::test]
async fn no_credential_means_not_verified_never_verified() {
    let (result, _http, _db, path) = discover_with(
        vec![(
            "/mail/config-v1.1.xml",
            200,
            mozilla_doc("example.com", "SSL"),
        )],
        request("ada@example.com"),
    )
    .await;
    let proposal = result.expect("a proposal");
    assert!(!proposal.login_validated);
    assert!(
        proposal.validation_detail.contains("no credential"),
        "detail: {}",
        proposal.validation_detail
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_starttls_only_server_is_reported_unverified_and_warned_about() {
    // rmail's IMAP client speaks implicit TLS only, so the real login probe
    // declines rather than pretending, and the proposal says so up front.
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "STARTTLS"),
    )])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(TlsLoginProbe) as Arc<dyn LoginProbe>,
    );
    let proposal = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential: CredentialSource::Command("printf hunter2".to_owned()),
                allow_model_fallback: false,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a proposal");

    assert_eq!(proposal.imap.security, Security::StartTls);
    assert!(!proposal.login_validated);
    assert!(
        proposal
            .warnings
            .iter()
            .any(|w| w.contains("implicit TLS only")),
        "warnings: {:?}",
        proposal.warnings
    );
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// The TOML block
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_rendered_block_is_accepted_by_the_real_config_parser() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let proposal = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                // A credential reference with a quote and a newline in it: a
                // hand-formatted block would produce a broken (or hostile)
                // file, and the point of serializing is that it cannot.
                credential: CredentialSource::Command(
                    "pass show \"mail/ada\"\n# not a comment".to_owned(),
                ),
                allow_model_fallback: false,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a proposal");

    let parsed: Config = toml::from_str(&proposal.toml).expect("the block must be valid config");
    let account = parsed.accounts.first().expect("one account");
    assert_eq!(account.name, "ada@example.com");
    assert_eq!(account.imap_server.as_deref(), Some("imap.example.com"));
    assert_eq!(account.port, 993);
    assert_eq!(account.username.as_deref(), Some("ada@example.com"));
    assert_eq!(account.smtp_server.as_deref(), Some("smtp.example.com"));
    assert_eq!(account.smtp_port, 587);
    assert_eq!(
        account.password_command.as_deref(),
        Some("pass show \"mail/ada\"\n# not a comment"),
        "the credential must round-trip through TOML unchanged"
    );
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// An account that already exists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_existing_account_is_reported_and_left_alone() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let existing = crate::account::create(
        &db,
        crate::account::NewAccount {
            name: "ada@example.com".to_owned(),
            imap_server: Some("legacy.example.com".to_owned()),
            imap_port: Some(143),
            username: Some("ada@example.com".to_owned()),
            ..Default::default()
        },
    )
    .await
    .expect("seed an account");

    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let proposal = engine
        .discover(&request("ada@example.com"), &CancellationToken::new())
        .await
        .expect("a proposal");

    assert_eq!(proposal.existing_account_id, Some(existing.id));
    assert!(
        proposal
            .warnings
            .iter()
            .any(|w| w.contains("already configured")),
        "warnings: {:?}",
        proposal.warnings
    );
    assert!(
        proposal
            .warnings
            .iter()
            .any(|w| w.contains("legacy.example.com")),
        "the difference must be spelled out: {:?}",
        proposal.warnings
    );

    // And nothing was changed: an autoconfiguration proposes, it does not
    // rewrite an account's settings underneath its owner.
    let after = crate::account::get(&db, existing.id).await.expect("reread");
    assert_eq!(after.imap_server.as_deref(), Some("legacy.example.com"));
    assert_eq!(after.imap_port, Some(143));
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// The model fallback
// ---------------------------------------------------------------------------

/// A discovery with no probe hits and a model wired in.
async fn discover_with_model(
    provider: Arc<MockProvider>,
    config: Config,
    allow: bool,
) -> (Result<Proposal, Error>, Database, PathBuf) {
    let (result, _, db, path) =
        discover_with_model_and_credential(provider, config, allow, CredentialSource::None).await;
    (result, db, path)
}

/// The same, with a credential — so a test can assert what the login probe
/// was (or was not) asked to do. Returns the probe alongside the result.
async fn discover_with_model_and_credential(
    provider: Arc<MockProvider>,
    config: Config,
    allow: bool,
    credential: CredentialSource,
) -> (Result<Proposal, Error>, Arc<StubLogin>, Database, PathBuf) {
    let http = Http::start(vec![(
        "/dns-query",
        200,
        r#"{"Answer":[{"type":15,"data":"10 mx.provider.example."}]}"#.to_owned(),
    )])
    .await;
    let (db, path) = open_db().await;
    let login = Arc::new(StubLogin::default());
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::clone(&login) as Arc<dyn LoginProbe>,
    )
    .with_inferrer(inferrer(&db, provider, &config));
    let result = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential,
                allow_model_fallback: allow,
            },
            &CancellationToken::new(),
        )
        .await;
    // `http` is kept alive until here on purpose: dropping it closes the
    // listener the probes are talking to.
    drop(http);
    (result, login, db, path)
}

fn good_answer() -> String {
    serde_json::json!({
        "imap_host": "imap.provider.example",
        "imap_port": 993,
        "imap_security": "tls",
        "smtp_host": "smtp.provider.example",
        "smtp_port": 465,
        "smtp_security": "tls",
        "confident": true
    })
    .to_string()
}

#[tokio::test]
async fn the_model_fallback_only_runs_when_it_is_asked_for() {
    let provider = MockProvider::answering(&good_answer());
    let (result, _db, path) =
        discover_with_model(Arc::clone(&provider), Config::default(), false).await;
    assert_eq!(
        result.expect_err("not asked for").reason(),
        ErrorReason::NotFound
    );
    assert_eq!(provider.calls(), 0, "the model must not have been called");
    cleanup(&path);
}

#[tokio::test]
async fn a_model_proposal_is_returned_as_a_proposal_and_warned_about() {
    let provider = MockProvider::answering(&good_answer());
    let (result, _db, path) =
        discover_with_model(Arc::clone(&provider), Config::default(), true).await;
    let proposal = result.expect("a proposal");
    assert_eq!(proposal.source, Source::Model);
    assert_eq!(proposal.imap.host, "imap.provider.example");
    assert_eq!(proposal.imap.port, 993);
    assert_eq!(proposal.imap.security, Security::Tls);
    assert!(
        proposal
            .warnings
            .iter()
            .any(|w| w.contains("proposed by a language model")),
        "warnings: {:?}",
        proposal.warnings
    );
    assert_eq!(provider.calls(), 1);
    cleanup(&path);
}

#[tokio::test]
async fn a_model_proposal_is_validated_exactly_like_a_documents() {
    for (field, value) in [
        ("imap_host", serde_json::json!("127.0.0.1")),
        ("imap_host", serde_json::json!("localhost")),
        ("imap_security", serde_json::json!("plain")),
        ("imap_port", serde_json::json!(0)),
        ("smtp_host", serde_json::json!("10.0.0.5")),
    ] {
        let mut answer: serde_json::Value =
            serde_json::from_str(&good_answer()).expect("valid fixture");
        answer[field] = value.clone();
        let provider = MockProvider::answering(&answer.to_string());
        let (result, _db, path) =
            discover_with_model(Arc::clone(&provider), Config::default(), true).await;
        let error = result.expect_err(&format!("{field} = {value} must be refused"));
        assert_eq!(
            error.reason(),
            ErrorReason::FailedPrecondition,
            "{field} = {value} gave {error}"
        );
        cleanup(&path);
    }
}

#[tokio::test]
async fn an_unconfident_model_is_not_a_configuration() {
    let mut answer: serde_json::Value = serde_json::from_str(&good_answer()).expect("fixture");
    answer["confident"] = serde_json::json!(false);
    let provider = MockProvider::answering(&answer.to_string());
    let (result, _db, path) =
        discover_with_model(Arc::clone(&provider), Config::default(), true).await;
    assert_eq!(
        result.expect_err("an unconfident guess").reason(),
        ErrorReason::NotFound
    );
    cleanup(&path);
}

#[tokio::test]
async fn the_model_prompt_is_fenced_and_never_carries_the_local_part() {
    let provider = MockProvider::answering(&good_answer());
    let (result, _db, path) =
        discover_with_model(Arc::clone(&provider), Config::default(), true).await;
    result.expect("a proposal");

    let system = provider.last_system();
    assert!(
        system.contains(crate::ai::injection::DATA_BOUNDARY_CLAUSE),
        "the system prompt must carry the data boundary: {system}"
    );
    let transmitted = provider.transmitted();
    // Every piece of evidence, in a block the boundary clause names.
    for label in ["mail-domain", "mx-records", "autoconfig", "ispdb", "dns"] {
        assert!(
            transmitted.contains(&format!("⟪untrusted {label}⟫"))
                && transmitted.contains(&format!("⟪/untrusted {label}⟫")),
            "the {label} evidence must be fenced on both sides: {transmitted}"
        );
    }
    assert!(
        transmitted.contains("⟪untrusted mail-domain⟫\nexample.com\n⟪/untrusted mail-domain⟫"),
        "the domain is fenced evidence: {transmitted}"
    );
    // The one thing that must never be in there: whose mailbox this is.
    assert!(
        !transmitted.contains("ada"),
        "the local part reached the model: {transmitted}"
    );
    cleanup(&path);
}

#[tokio::test]
async fn the_model_fallback_honors_the_ai_policy() {
    // `ai.enabled = false` forbids every target, including one that names no
    // configured account — which is the case autoconfig is, since it runs
    // before the account exists.
    let mut config = Config::default();
    config.ai.enabled = false;
    let provider = MockProvider::answering(&good_answer());
    let (result, _db, path) = discover_with_model(Arc::clone(&provider), config, true).await;
    assert_eq!(
        result.expect_err("policy forbids it").reason(),
        ErrorReason::FailedPrecondition
    );
    assert_eq!(
        provider.calls(),
        0,
        "policy must be resolved before anything is sent"
    );
    cleanup(&path);
}

#[tokio::test]
async fn the_model_fallback_is_declined_when_no_provider_is_wired() {
    let http = Http::start(vec![]).await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let error = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential: CredentialSource::None,
                allow_model_fallback: true,
            },
            &CancellationToken::new(),
        )
        .await
        .expect_err("no provider");
    // Declined explicitly, rather than silently answering "nothing found" to
    // a caller who asked for a guess.
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);
    assert!(error.to_string().contains("no AI provider"));
    cleanup(&path);
}

#[tokio::test]
async fn a_model_call_lands_in_the_audit_ledger() {
    let provider = MockProvider::answering(&good_answer());
    let (result, db, path) =
        discover_with_model(Arc::clone(&provider), Config::default(), true).await;
    result.expect("a proposal");
    let (rows, pass): (i64, Option<String>) = db
        .read(|conn| {
            conn.query_row("SELECT COUNT(*), MAX(pass) FROM ai_ledger", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
        })
        .await
        .expect("read the ledger");
    // Spend that is never recorded is spend the next budget check cannot see.
    assert_eq!(rows, 1);
    assert_eq!(pass.as_deref(), Some(infer::PASS));
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_discovery_stops_rather_than_finishing() {
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = engine
        .discover(&request("ada@example.com"), &cancel)
        .await
        .expect_err("a cancelled discovery must not answer");
    assert_eq!(error.reason(), ErrorReason::Cancelled);
    cleanup(&path);
}

/// A place to keep the type-level assertion that nothing outside this module
/// can build settings that skip the validator.
#[test]
fn settings_carry_no_plaintext_variant() {
    // If a `Security::Plain` is ever added, this stops compiling — which is
    // the intent: the absence of the variant is the guarantee.
    let all: HashMap<&str, Security> = [("tls", Security::Tls), ("starttls", Security::StartTls)]
        .into_iter()
        .collect();
    assert_eq!(all.len(), 2);
    for (spelling, security) in all {
        assert_eq!(security.as_str(), spelling);
    }
}

// ---------------------------------------------------------------------------
// The gaps a review found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_is_never_followed() {
    // Every URL here is derived from the address being configured, and every
    // response body becomes evidence — including evidence a model reads. With
    // `reqwest`'s default policy this 302 would be followed (up to ten hops,
    // any scheme, any host), and the target's body would come back as the
    // domain's autoconfiguration. It is not followed, so a 3xx is simply not
    // a hit and the pass continues to the next probe.
    let (result, http, _db, path) = discover_with(
        vec![
            ("/mail/config-v1.1.xml", 302, "/redirected".to_owned()),
            (
                "/redirected",
                200,
                mozilla_doc("elsewhere.example.com", "SSL"),
            ),
        ],
        request("ada@example.com"),
    )
    .await;

    assert_eq!(
        result.expect_err("a 302 is not a configuration").reason(),
        ErrorReason::NotFound
    );
    assert!(
        !http.requests().iter().any(|r| r.starts_with("/redirected")),
        "the redirect was followed: {:?}",
        http.requests()
    );
    cleanup(&path);
}

#[test]
fn a_malformed_srv_record_does_not_discard_the_good_ones() {
    // These arrive as a set. One truncated `data` string must not make the
    // whole lookup a miss.
    let body = r#"{"Status":0,"Answer":[
        {"type":33,"data":"10 5 993"},
        {"type":33,"data":"not even close"},
        {"type":33,"data":"10 5 993 imap.example.com."},
        {"type":33,"data":"90 1 993 backup.example.com."}
    ]}"#;
    assert_eq!(
        probe::parse_srv(body),
        Some(("imap.example.com.".to_owned(), 993))
    );
    // A weight at the edge of the wire type orders correctly and overflows
    // nothing — the previous spelling negated an `i64` read off the network.
    let extreme = r#"{"Status":0,"Answer":[
        {"type":33,"data":"0 65535 993 heavy.example.com."},
        {"type":33,"data":"0 0 993 light.example.com."}
    ]}"#;
    assert_eq!(
        probe::parse_srv(extreme),
        Some(("heavy.example.com.".to_owned(), 993))
    );
}

#[test]
fn a_refusal_never_quotes_an_unbounded_value() {
    // The quoted value is attacker-controlled text on its way into a `Status`
    // message and a log line; a 256 KiB `<socketType>` element is an ordinary
    // thing for a hostile document to contain.
    let huge = "x".repeat(300_000);
    for message in [
        Security::parse(&huge).expect_err("unknown").to_string(),
        validate::host(&huge).expect_err("too long").to_string(),
    ] {
        assert!(
            message.len() < 1_000,
            "a refusal quoted {} bytes of someone else's text",
            message.len()
        );
    }
}

#[test]
fn a_hostile_username_template_falls_back_to_the_address() {
    let address = Address::parse("ada@example.com").expect("valid");
    let long = "a".repeat(500);
    for hostile in [
        // Reverses everything printed after it, and is *not* a control char,
        // so a `is_control` filter would have let it through.
        "ada\u{202e}moc.elpmaxe",
        long.as_str(),
        "ada example",
        "ada\u{0}",
    ] {
        assert_eq!(
            address.expand_username(Some(hostile)),
            "ada@example.com",
            "{hostile:?} was accepted as a literal username"
        );
    }
    // A plain literal username is still honored — providers do publish them.
    assert_eq!(
        address.expand_username(Some("ada.lovelace")),
        "ada.lovelace"
    );
    assert_eq!(address.expand_username(Some("%EMAILLOCALPART%")), "ada");
    assert_eq!(address.expand_username(None), "ada@example.com");
}

#[tokio::test]
async fn a_model_proposed_host_is_never_sent_the_password() {
    // `allow_model_fallback` is consent to *ask* the model, not consent to
    // present the user's credential to whatever hostname it produced from a
    // corpus of attacker-controlled probe responses.
    let provider = MockProvider::answering(&good_answer());
    let (result, login, _db, path) = discover_with_model_and_credential(
        Arc::clone(&provider),
        Config::default(),
        true,
        CredentialSource::Command("printf hunter2".to_owned()),
    )
    .await;
    let proposal = result.expect("a proposal");

    assert_eq!(proposal.source, Source::Model);
    assert!(
        login.attempts().is_empty(),
        "the credential was presented to a model-named host: {:?}",
        login.attempts()
    );
    assert!(!proposal.login_validated);
    assert!(
        proposal.validation_detail.contains("proposed by a model"),
        "detail: {}",
        proposal.validation_detail
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_probe_hit_with_a_credential_is_still_verified() {
    // The mirror of the test above: the refusal is specific to the model, not
    // a verification path that quietly stopped running for everyone.
    let http = Http::start(vec![(
        "/mail/config-v1.1.xml",
        200,
        mozilla_doc("example.com", "SSL"),
    )])
    .await;
    let (db, path) = open_db().await;
    let login = Arc::new(StubLogin::default());
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::clone(&login) as Arc<dyn LoginProbe>,
    );
    let proposal = engine
        .discover(
            &AutoconfigRequest {
                email: "ada@example.com".to_owned(),
                credential: CredentialSource::Command("printf hunter2".to_owned()),
                allow_model_fallback: false,
            },
            &CancellationToken::new(),
        )
        .await
        .expect("a proposal");

    assert!(proposal.login_validated);
    assert_eq!(
        login.attempts(),
        vec![(
            "imap.example.com".to_owned(),
            993,
            "ada@example.com".to_owned()
        )]
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_per_account_ai_opt_out_reaches_a_call_made_before_the_account_loads() {
    // The claim `gate::admit_unattributed` makes: an operator who turned AI
    // off for this address gets that honored by the one call that runs before
    // the account is loaded. Resolving against anything but the address —
    // an empty target, a constant — would silently ignore the opt-out, and
    // the *global* `ai.enabled` switch cannot prove otherwise because it
    // short-circuits before the target is ever read.
    let config: Config = toml::from_str(
        r#"
[[accounts]]
name = "ada@example.com"
ai = { enabled = false }
"#,
    )
    .expect("a valid config");
    let provider = MockProvider::answering(&good_answer());
    let (result, _db, path) = discover_with_model(Arc::clone(&provider), config, true).await;

    assert_eq!(
        result.expect_err("this account has AI disabled").reason(),
        ErrorReason::FailedPrecondition
    );
    assert_eq!(
        provider.calls(),
        0,
        "policy must be resolved before anything is sent"
    );
    cleanup(&path);
}

#[tokio::test]
async fn a_probe_that_never_answers_is_cut_short_by_cancellation() {
    // Not a pre-cancelled token: this one fires while a probe is parked on a
    // server that accepted the connection and will never reply. Without the
    // cancellation arms inside the request and the body read, this waits out
    // every probe's own timeout instead.
    let http = Http::start(vec![
        ("/mail/config-v1.1.xml", HANG_STATUS, String::new()),
        ("/ispdb", HANG_STATUS, String::new()),
        ("/autodiscover", HANG_STATUS, String::new()),
        ("/dns-query", HANG_STATUS, String::new()),
    ])
    .await;
    let (db, path) = open_db().await;
    let engine = Autoconfigurator::with_parts(
        db.clone(),
        Arc::new(Probes::new(http.endpoints()).expect("probes")),
        Arc::new(StubLogin::default()) as Arc<dyn LoginProbe>,
    );
    let cancel = CancellationToken::new();
    let fired = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        fired.cancel();
    });

    let started = std::time::Instant::now();
    let error = engine
        .discover(&request("ada@example.com"), &cancel)
        .await
        .expect_err("a cancelled discovery must not answer");
    let elapsed = started.elapsed();

    assert_eq!(error.reason(), ErrorReason::Cancelled);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cancellation took {elapsed:?}; the probe waited out its own timeout instead"
    );
    cleanup(&path);
}
