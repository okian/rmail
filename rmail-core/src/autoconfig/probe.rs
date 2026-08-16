//! The network probes: the domain's own autoconfig document, Mozilla's ISPDB,
//! Microsoft autodiscover, and RFC 6186 SRV records.
//!
//! # Everything here is an HTTP request, including the DNS
//!
//! SRV and MX lookups go through DNS-over-HTTPS (the JSON API Google and
//! Cloudflare both serve) rather than a DNS client library. Two reasons, in
//! order of importance:
//!
//! 1. **It is testable without a network.** Every probe in this module is
//!    then a request to a URL, and every URL is overridable, so the whole
//!    discovery path can be driven against a loopback server that speaks
//!    exactly the responses a test wants — the pattern `ai::provider` and
//!    `embed::voyage` already use. A resolver that read `/etc/resolv.conf`
//!    would leave the SRV path either untested or tested against whatever
//!    the machine running the suite happens to resolve.
//! 2. It adds no dependency: `reqwest` is already here for the model provider
//!    and the embedder, and a DNS client would be a new one to audit.
//!
//! # Nothing here decides anything
//!
//! These functions parse documents and return what they said. Whether a value
//! is *usable* is [`super::validate`]'s job, and it is applied to every
//! candidate before it becomes a [`super::ServerSettings`] — including the
//! ones from the model. A probe that returns a hostile document is not a
//! failure mode this module has to detect; it is one the validator refuses.
//!
//! # Bounded on purpose
//!
//! A response body is read to a cap ([`MAX_BODY_BYTES`]) rather than to
//! completion, each request carries its own timeout, and every await races the
//! caller's cancellation token. The servers on the other end are chosen by the
//! address being configured, which is to say by someone else.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::Error;

use super::{Security, Source};

/// Mozilla's ISPDB, the database Thunderbird ships against.
const DEFAULT_ISPDB_BASE: &str = "https://autoconfig.thunderbird.net/v1.1";

/// A DNS-over-HTTPS resolver speaking the JSON API (RFC 8484's JSON sibling).
const DEFAULT_DOH_ENDPOINT: &str = "https://dns.google/resolve";

/// Per-request timeout. Short: four probes run in sequence behind a user
/// waiting at a prompt, and a provider that has not answered in this long is
/// not going to answer usefully.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of any response body that is read.
///
/// An autoconfig document is a page of XML; a DoH answer is a few hundred
/// bytes. This is two orders of magnitude of headroom over both, and it is
/// what stops a hostile endpoint from streaming until the daemon runs out of
/// memory.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// The DNS `TYPE` codes used here (RFC 1035 / RFC 2782).
const TYPE_MX: u16 = 15;
const TYPE_SRV: u16 = 33;

/// Where each probe looks.
///
/// Every field is overridable so the suite can point the whole discovery path
/// at a loopback server. The defaults are the real endpoints; nothing in
/// production overrides them.
#[derive(Debug, Clone)]
pub struct ProbeEndpoints {
    /// ISPDB base; the domain is appended (`{base}/{domain}`).
    pub ispdb_base: String,
    /// Base URL for the *per-domain* documents. `None` derives the real ones
    /// from the domain being configured
    /// (`https://autoconfig.<domain>/mail/config-v1.1.xml` and
    /// `https://autodiscover.<domain>/autodiscover/autodiscover.xml`).
    pub domain_base: Option<String>,
    /// DNS-over-HTTPS JSON endpoint, queried as `{endpoint}?name=..&type=..`.
    pub doh_endpoint: String,
}

impl Default for ProbeEndpoints {
    fn default() -> Self {
        Self {
            ispdb_base: DEFAULT_ISPDB_BASE.to_owned(),
            domain_base: None,
            doh_endpoint: DEFAULT_DOH_ENDPOINT.to_owned(),
        }
    }
}

/// One probe's response, kept verbatim as evidence for the model fallback.
///
/// Untrusted text, and typed as such by where it is allowed to go: the only
/// consumer is [`super::infer`], which wraps it in
/// [`crate::ai::injection::untrusted_block`].
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    /// Which probe produced it (`"autoconfig"`, `"ispdb"`, ...).
    pub probe: &'static str,
    /// The URL asked.
    pub url: String,
    /// The HTTP status, or 0 for a request that never got one.
    pub status: u16,
    /// The body, truncated to [`MAX_BODY_BYTES`] and lossily decoded.
    pub body: String,
}

/// A candidate pair of servers, exactly as a document stated them — before
/// validation.
#[derive(Debug, Clone)]
pub struct RawCandidate {
    /// Which probe found it.
    pub source: Source,
    /// Incoming (IMAP) server: host, port, socket type, username template.
    pub imap: RawServer,
    /// Outgoing (SMTP) server, when the document named one.
    pub smtp: Option<RawServer>,
}

/// One server as a document stated it. Every field is a string because every
/// field is someone else's text until [`super::validate`] has seen it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawServer {
    /// Hostname as written.
    pub host: String,
    /// Port as written.
    pub port: String,
    /// Socket type as written (`SSL`, `STARTTLS`, `plain`, ...).
    pub security: String,
    /// Username template (`%EMAILADDRESS%`, `%EMAILLOCALPART%`), if given.
    pub username: Option<String>,
}

/// What a full discovery pass found.
#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    /// The first probe that produced a usable-looking candidate, if any.
    pub candidate: Option<RawCandidate>,
    /// Every response seen, in the order the probes ran — the evidence the
    /// model fallback is given.
    pub responses: Vec<ProbeResponse>,
    /// The domain's MX hosts, best-effort.
    pub mx: Vec<String>,
}

/// The probe runner.
#[derive(Debug)]
pub struct Probes {
    client: reqwest::Client,
    endpoints: ProbeEndpoints,
}

impl Probes {
    /// Build a runner over its own HTTP client.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the HTTP client cannot be built.
    pub fn new(endpoints: ProbeEndpoints) -> Result<Self, Error> {
        // As in the IMAP client, the Voyage embedder and the Claude provider:
        // chosen explicitly, because inference fails at runtime on the first
        // handshake once anything pulls in a second provider.
        crate::transport::install_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // No redirects, the same rule (and for a sharper reason) than
            // `crate::oauth`'s client follows. Every URL here is derived from
            // the address being configured, and every response body becomes
            // evidence — including evidence handed to a model. `reqwest`'s
            // default policy follows up to ten hops with no scheme or host
            // restriction, so a document served by the domain under
            // configuration could redirect this client to
            // `http://169.254.169.254/…`, or to any host inside the network
            // this daemon runs in, and the body would come back as a probe
            // response. A 3xx is simply not a hit.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                Error::failed_precondition(format!("could not build an HTTP client: {e}"))
            })?;
        Ok(Self { client, endpoints })
    }

    /// Run every probe against `domain`, in Thunderbird's order, stopping at
    /// the first that yields a candidate.
    ///
    /// Never fails: a probe that errors is evidence too, and the caller's next
    /// step (the model fallback, or an honest "not found") depends on seeing
    /// what each one said. Cancellation stops the pass wherever it is.
    #[tracing::instrument(skip(self, cancel), fields(domain = domain))]
    pub async fn run(&self, domain: &str, cancel: &CancellationToken) -> ProbeReport {
        let mut report = ProbeReport::default();

        // The domain's own document first: an administrator's statement about
        // their own domain outranks a third party's record of it.
        for (probe, url) in self.document_urls(domain) {
            if cancel.is_cancelled() {
                return report;
            }
            let response = self.get(probe, &url, cancel).await;
            if response.status == 200 {
                let parsed = match probe {
                    "autodiscover" => parse_autodiscover(&response.body),
                    _ => parse_mozilla_autoconfig(&response.body),
                };
                if let Some(mut candidate) = parsed {
                    candidate.source = source_for(probe);
                    report.candidate = Some(candidate);
                    report.responses.push(response);
                    return report;
                }
            }
            report.responses.push(response);
        }

        // RFC 6186 SRV, last of the deterministic probes: it names a host and
        // a port but never a username, so it is the weakest of the three even
        // when it answers.
        if !cancel.is_cancelled() {
            if let Some(candidate) = self.srv_candidate(domain, &mut report, cancel).await {
                report.candidate = Some(candidate);
                return report;
            }
        }

        // Only reached on a miss, and only used by the model fallback.
        if !cancel.is_cancelled() {
            report.mx = self.mx_hosts(domain, &mut report, cancel).await;
        }
        report
    }

    /// The document URLs to try, in order.
    fn document_urls(&self, domain: &str) -> Vec<(&'static str, String)> {
        match &self.endpoints.domain_base {
            // A test points every per-domain document at one server.
            Some(base) => {
                let base = base.trim_end_matches('/');
                vec![
                    ("autoconfig", format!("{base}/mail/config-v1.1.xml")),
                    (
                        "ispdb",
                        format!(
                            "{}/{domain}",
                            self.endpoints.ispdb_base.trim_end_matches('/')
                        ),
                    ),
                    (
                        "autodiscover",
                        format!("{base}/autodiscover/autodiscover.xml"),
                    ),
                ]
            }
            None => vec![
                (
                    "autoconfig",
                    format!("https://autoconfig.{domain}/mail/config-v1.1.xml"),
                ),
                (
                    "ispdb",
                    format!(
                        "{}/{domain}",
                        self.endpoints.ispdb_base.trim_end_matches('/')
                    ),
                ),
                (
                    "autodiscover",
                    format!("https://autodiscover.{domain}/autodiscover/autodiscover.xml"),
                ),
            ],
        }
    }

    /// RFC 6186: `_imaps._tcp` / `_imap._tcp` for incoming,
    /// `_submissions._tcp` / `_submission._tcp` for outgoing.
    async fn srv_candidate(
        &self,
        domain: &str,
        report: &mut ProbeReport,
        cancel: &CancellationToken,
    ) -> Option<RawCandidate> {
        let imap = self
            .first_srv(
                &[
                    (format!("_imaps._tcp.{domain}"), Security::Tls),
                    (format!("_imap._tcp.{domain}"), Security::StartTls),
                ],
                report,
                cancel,
            )
            .await?;
        let smtp = self
            .first_srv(
                &[
                    (format!("_submissions._tcp.{domain}"), Security::Tls),
                    (format!("_submission._tcp.{domain}"), Security::StartTls),
                ],
                report,
                cancel,
            )
            .await;
        Some(RawCandidate {
            source: Source::Srv,
            imap,
            smtp,
        })
    }

    /// The first of `names` that yields a usable SRV record.
    async fn first_srv(
        &self,
        names: &[(String, Security)],
        report: &mut ProbeReport,
        cancel: &CancellationToken,
    ) -> Option<RawServer> {
        for (name, security) in names {
            if cancel.is_cancelled() {
                return None;
            }
            let response = self.doh(name, TYPE_SRV, cancel).await;
            let found = parse_srv(&response.body);
            report.responses.push(response);
            if let Some((host, port)) = found {
                return Some(RawServer {
                    host,
                    port: port.to_string(),
                    security: security.as_str().to_owned(),
                    // SRV says where, never who: RFC 6186 §3 leaves the
                    // username to the client, and guessing it wrong is a
                    // failed login rather than a wrong one.
                    username: None,
                });
            }
        }
        None
    }

    /// The domain's MX hosts — evidence for the model, never a server to
    /// connect to.
    async fn mx_hosts(
        &self,
        domain: &str,
        report: &mut ProbeReport,
        cancel: &CancellationToken,
    ) -> Vec<String> {
        let response = self.doh(domain, TYPE_MX, cancel).await;
        let hosts = parse_mx(&response.body);
        report.responses.push(response);
        hosts
    }

    /// One DNS-over-HTTPS query.
    async fn doh(&self, name: &str, rtype: u16, cancel: &CancellationToken) -> ProbeResponse {
        let url = format!(
            "{}?name={}&type={rtype}",
            self.endpoints.doh_endpoint,
            urlencode(name)
        );
        let mut response = self.get("dns", &url, cancel).await;
        // Distinguishable in the evidence handed to the model: "the SRV
        // lookup for _imaps._tcp" reads very differently from "some DNS query".
        response.url = format!("{url} ({name} type {rtype})");
        response
    }

    /// GET a URL, bounded and cancellable. Errors become responses with a
    /// zero status and the error text as the body: a probe that could not be
    /// reached is evidence, not an exception.
    async fn get(
        &self,
        probe: &'static str,
        url: &str,
        cancel: &CancellationToken,
    ) -> ProbeResponse {
        let request = self.client.get(url).header(
            reqwest::header::ACCEPT,
            "application/dns-json, text/xml, */*",
        );
        let sent = tokio::select! {
            () = cancel.cancelled() => {
                return ProbeResponse {
                    probe,
                    url: url.to_owned(),
                    status: 0,
                    body: "cancelled".to_owned(),
                };
            }
            result = request.send() => result,
        };
        match sent {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = read_capped(response, cancel).await;
                tracing::debug!(probe, url, status, bytes = body.len(), "autoconfig probe");
                ProbeResponse {
                    probe,
                    url: url.to_owned(),
                    status,
                    body,
                }
            }
            Err(error) => {
                tracing::debug!(probe, url, %error, "autoconfig probe failed");
                ProbeResponse {
                    probe,
                    url: url.to_owned(),
                    status: 0,
                    body: error.to_string(),
                }
            }
        }
    }
}

/// Read a response body to [`MAX_BODY_BYTES`], chunk by chunk.
///
/// Chunked rather than `Response::bytes()`, because the cap has to hold
/// against a server that never sets `Content-Length` and never stops sending.
async fn read_capped(mut response: reqwest::Response, cancel: &CancellationToken) -> String {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => break,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(bytes)) => {
                let room = MAX_BODY_BYTES.saturating_sub(body.len());
                if room == 0 {
                    tracing::debug!("autoconfig response exceeded its body cap; truncating");
                    break;
                }
                body.extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            // A body that broke mid-read is still evidence of what arrived.
            Ok(None) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Percent-encode the few characters a DNS name could contain that would
/// change the meaning of the query string.
///
/// Deliberately an allowlist: anything that is not an unreserved DNS-name
/// character is escaped, so a "name" carrying `&type=A#` cannot re-aim the
/// query.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn source_for(probe: &str) -> Source {
    match probe {
        "ispdb" => Source::Ispdb,
        "autodiscover" => Source::Autodiscover,
        _ => Source::Autoconfig,
    }
}

/// Parse a Mozilla autoconfig document (the format both the per-domain
/// document and the ISPDB serve).
///
/// Takes the first `incomingServer type="imap"` and the first
/// `outgoingServer type="smtp"`; a document offering only POP3 yields nothing,
/// which is correct — this client speaks IMAP.
pub fn parse_mozilla_autoconfig(xml: &str) -> Option<RawCandidate> {
    use quick_xml::events::Event;

    #[derive(Default)]
    struct Partial {
        kind: Option<String>,
        host: Option<String>,
        port: Option<String>,
        security: Option<String>,
        username: Option<String>,
    }

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buffer = Vec::new();
    let mut imap: Option<RawServer> = None;
    let mut smtp: Option<RawServer> = None;
    let mut current: Option<Partial> = None;
    let mut field: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                let name = local_name(start.name().as_ref());
                match name.as_str() {
                    "incomingserver" | "outgoingserver" => {
                        let kind = start
                            .attributes()
                            .flatten()
                            .find(|a| local_name(a.key.as_ref()) == "type")
                            .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
                            .map(|v| v.to_ascii_lowercase());
                        current = Some(Partial {
                            kind,
                            ..Partial::default()
                        });
                    }
                    "hostname" | "port" | "sockettype" | "username" => field = Some(name),
                    _ => field = None,
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(partial), Some(field)) = (current.as_mut(), field.as_deref()) {
                    if let Ok(value) = text.decode() {
                        let value = value.trim().to_owned();
                        if !value.is_empty() {
                            match field {
                                "hostname" => partial.host = Some(value),
                                "port" => partial.port = Some(value),
                                "sockettype" => partial.security = Some(value),
                                _ => partial.username = Some(value),
                            }
                        }
                    }
                }
            }
            Ok(Event::End(end)) => {
                let name = local_name(end.name().as_ref());
                if name == "incomingserver" || name == "outgoingserver" {
                    if let Some(partial) = current.take() {
                        let slot = match (name.as_str(), partial.kind.as_deref()) {
                            ("incomingserver", Some("imap")) if imap.is_none() => Some(&mut imap),
                            ("outgoingserver", Some("smtp")) if smtp.is_none() => Some(&mut smtp),
                            _ => None,
                        };
                        if let (Some(slot), Some(host)) = (slot, partial.host) {
                            *slot = Some(RawServer {
                                host,
                                port: partial.port.unwrap_or_default(),
                                security: partial.security.unwrap_or_default(),
                                username: partial.username,
                            });
                        }
                    }
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            // A malformed document yields whatever was already complete —
            // the same "partial is better than nothing" rule the attachment
            // extractor applies, and the validator still has to accept it.
            Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    imap.map(|imap| RawCandidate {
        source: Source::Autoconfig,
        imap,
        smtp,
    })
}

/// Parse a Microsoft POX autodiscover response.
///
/// The shape is `Autodiscover/Response/Account/Protocol`, one `Protocol` per
/// service, with `Type` naming it (`IMAP`, `SMTP`) and `SSL` being `on`/`off`.
pub fn parse_autodiscover(xml: &str) -> Option<RawCandidate> {
    use quick_xml::events::Event;

    #[derive(Default)]
    struct Partial {
        kind: Option<String>,
        host: Option<String>,
        port: Option<String>,
        ssl: Option<String>,
        encryption: Option<String>,
        username: Option<String>,
    }

    impl Partial {
        fn into_server(self) -> Option<RawServer> {
            let host = self.host?;
            Some(RawServer {
                host,
                port: self.port.unwrap_or_default(),
                // `Encryption` is the newer, more specific element; `SSL`
                // is the old on/off flag. Prefer the specific one.
                security: self
                    .encryption
                    .or(self.ssl)
                    .unwrap_or_else(|| "off".to_owned()),
                username: self.username,
            })
        }
    }

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buffer = Vec::new();
    let mut imap: Option<RawServer> = None;
    let mut smtp: Option<RawServer> = None;
    let mut current: Option<Partial> = None;
    let mut field: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                let name = local_name(start.name().as_ref());
                match name.as_str() {
                    "protocol" => current = Some(Partial::default()),
                    "type" | "server" | "port" | "ssl" | "encryption" | "loginname" => {
                        field = Some(name);
                    }
                    _ => field = None,
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(partial), Some(field)) = (current.as_mut(), field.as_deref()) {
                    if let Ok(value) = text.decode() {
                        let value = value.trim().to_owned();
                        if !value.is_empty() {
                            match field {
                                "type" => partial.kind = Some(value.to_ascii_lowercase()),
                                "server" => partial.host = Some(value),
                                "port" => partial.port = Some(value),
                                "ssl" => partial.ssl = Some(value),
                                "encryption" => partial.encryption = Some(value),
                                _ => partial.username = Some(value),
                            }
                        }
                    }
                }
            }
            Ok(Event::End(end)) => {
                if local_name(end.name().as_ref()) == "protocol" {
                    if let Some(partial) = current.take() {
                        match partial.kind.as_deref() {
                            Some("imap") if imap.is_none() => imap = partial.into_server(),
                            Some("smtp") if smtp.is_none() => smtp = partial.into_server(),
                            _ => {}
                        }
                    }
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    imap.map(|imap| RawCandidate {
        source: Source::Autodiscover,
        imap,
        smtp,
    })
}

/// The best SRV target in a DoH JSON answer: lowest priority, then highest
/// weight, then lexicographically lowest target.
///
/// The last tiebreak is not in RFC 2782 (which says to pick randomly among
/// equal weights). It is here because a discovery that returns a different
/// server each time it is asked is untestable and unexplainable, and the
/// weighting exists for load balancing a live client, not for a one-shot
/// configuration guess.
///
/// A single record with target `.` is RFC 2782's "the service is decidedly
/// not available at this domain" and yields `None`.
///
/// A malformed record is skipped, never fatal: these arrive in a set, and one
/// truncated `data` string among five good records must not make all five
/// unusable.
pub fn parse_srv(body: &str) -> Option<(String, u16)> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let answers = json.get("Answer")?.as_array()?;
    let mut best: Option<(u32, std::cmp::Reverse<u16>, String, u16)> = None;
    for answer in answers {
        if answer.get("type").and_then(serde_json::Value::as_u64) != Some(u64::from(TYPE_SRV)) {
            continue;
        }
        let Some(fields) = srv_fields(answer) else {
            continue;
        };
        let (priority, weight, port, target) = fields;
        if target == "." {
            continue;
        }
        // Lowest priority wins; then highest weight; then the name, so the
        // answer is a function of the records and not of their order.
        // `Reverse` rather than a negation: the weight came off the network,
        // and negating an i64 read from someone else's string is an overflow
        // waiting to be sent one.
        let key = (priority, std::cmp::Reverse(weight), target, port);
        if best.as_ref().map_or(true, |current| key < *current) {
            best = Some(key);
        }
    }
    best.map(|(_, _, target, port)| (target, port))
}

/// `priority weight port target` from one SRV answer, or `None` if the record
/// is not that.
///
/// The wire types are RFC 2782's: three 16-bit unsigned fields and a name.
fn srv_fields(answer: &serde_json::Value) -> Option<(u32, u16, u16, String)> {
    let data = answer.get("data")?.as_str()?;
    let mut parts = data.split_whitespace();
    let priority: u32 = u32::from(parts.next()?.parse::<u16>().ok()?);
    let weight: u16 = parts.next()?.parse().ok()?;
    let port: u16 = parts.next()?.parse().ok()?;
    let target = parts.next()?.to_owned();
    Some((priority, weight, port, target))
}

/// The MX hostnames in a DoH JSON answer, best first.
pub fn parse_mx(body: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(answers) = json.get("Answer").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut hosts: Vec<(u32, String)> = answers
        .iter()
        .filter(|a| a.get("type").and_then(serde_json::Value::as_u64) == Some(u64::from(TYPE_MX)))
        .filter_map(|a| {
            let data = a.get("data")?.as_str()?;
            let (preference, host) = data.split_once(char::is_whitespace)?;
            Some((preference.parse().ok()?, host.trim().to_owned()))
        })
        .collect();
    hosts.sort();
    // Bounded: this is evidence handed to a model, and the model's prompt is
    // not a place to paste an unbounded list someone else controls.
    hosts.truncate(8);
    hosts.into_iter().map(|(_, host)| host).collect()
}

/// An XML element or attribute name with any namespace prefix removed, lowered.
fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    let local = name.rsplit(':').next().unwrap_or(&name);
    local.to_ascii_lowercase()
}
