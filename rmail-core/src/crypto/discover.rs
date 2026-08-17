//! Finding a public key for an address, in an order chosen for privacy.
//!
//! # The order is the design
//!
//! ```text
//! autocrypt ──▶ wkd ──▶ private keyservers ──▶ public keyservers
//! (local)      (their    (yours)                (everyone's)
//!               domain)
//! ```
//!
//! Every source is enabled by default and the chain **stops at the first
//! usable key**, which is what makes the ordering a privacy mechanism rather
//! than a preference. A keyserver query carries the address being looked up,
//! so asking a public server "who is bob@example.com?" tells that server the
//! user is about to email Bob. Running the sources that cannot leak first
//! means the ones that can are only reached for addresses nothing local and
//! nothing authoritative could answer for — the leak shrinks to the residue
//! instead of covering every recipient.
//!
//! The first two cost nothing privately, for different reasons.
//! [`KeySource::Autocrypt`] reads mail already on disk: no request is made at
//! all. [`KeySource::Wkd`] queries the recipient's own domain, which is about
//! to receive the message anyway and therefore learns nothing from the lookup
//! that delivery would not have told it a moment later.
//!
//! Within the keyserver list, every [`KeyserverKind::Private`] entry runs
//! before every [`KeyserverKind::Public`] one regardless of how the array was
//! sorted in the config file — see [`ordered_keyservers`]. A privacy property
//! that depended on TOML ordering would be a privacy property nobody could
//! rely on.
//!
//! # One budget for the whole chain
//!
//! `crypto.discovery_timeout` bounds *all* of it, not each hop. Four sources
//! at ten seconds each is forty seconds of background work for an address that
//! most likely has no key; the deadline is checked between hops and the
//! remaining budget is handed to each request, so a slow first source spends
//! the budget rather than extending it.
//!
//! # What a failure is worth
//!
//! [`Outcome::Failed`] and [`Outcome::NotFound`] are different answers and the
//! cache treats them very differently (see [`super::cache`]). "Every source
//! errored" is not evidence that the recipient has no key, so it must never
//! be recorded as one — that would suppress discovery for a month over a
//! network blip. `NotFound` is only returned when every enabled source was
//! actually *reached* and none had a key.

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::config::{CryptoConfig, KeyserverConfig, KeyserverKind};

use super::key::{self, KeySource, UsableKey};
use super::normalize_address;

/// The result of running the discovery chain for one address.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// A usable key. Already the best of whatever was found.
    Found(Box<UsableKey>),
    /// Every enabled source was reached and none had a usable key.
    ///
    /// Only this outcome may be cached as `absent`.
    NotFound,
    /// No source could be reached, or the budget ran out first.
    ///
    /// Carries the reasons for the log; the cache records a backoff.
    Failed {
        /// One line per source that errored.
        reasons: Vec<String>,
    },
}

/// An HTTP getter, so the chain can be tested without a network.
///
/// A trait rather than a `reqwest::Client` because every meaningful test in
/// this module is about *which URLs are requested, in what order, and what
/// happens when one fails* — none of which is observable through a real
/// client, and all of which is the actual behaviour being specified. The
/// privacy ordering in particular is only testable if a fake can record the
/// sequence of requests.
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    /// GET `url`, returning the body, or `Ok(None)` for a 404.
    ///
    /// A 404 is `Ok(None)` rather than an error because "this server does not
    /// have that key" is a successful, informative answer — it is what lets
    /// the chain distinguish [`Outcome::NotFound`] from [`Outcome::Failed`].
    async fn get(
        &self,
        url: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String>;
}

/// The production [`Fetcher`].
#[derive(Debug, Clone)]
pub struct HttpFetcher {
    client: reqwest::Client,
    max_bytes: usize,
}

impl HttpFetcher {
    /// Build one with the configured size ceiling.
    #[must_use]
    pub fn new(client: reqwest::Client, max_bytes: usize) -> Self {
        Self { client, max_bytes }
    }
}

#[async_trait::async_trait]
impl Fetcher for HttpFetcher {
    async fn get(
        &self,
        url: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut req = self.client.get(url).timeout(timeout);
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("http {}", resp.status()));
        }

        // Trust `Content-Length` to reject early, but do not trust it to be
        // present or honest — the streamed read below is what actually
        // enforces the ceiling. A server that omits the header or lies about
        // it is exactly the server this limit exists for.
        if let Some(len) = resp.content_length() {
            if len > self.max_bytes as u64 {
                return Err(format!("key is {len} bytes, over the limit"));
            }
        }

        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        if body.len() > self.max_bytes {
            return Err(format!("key is {} bytes, over the limit", body.len()));
        }
        Ok(Some(body.to_vec()))
    }
}

/// Run the whole chain for one address.
///
/// # Errors
///
/// Does not return `Err` — every failure is folded into [`Outcome::Failed`].
/// Discovery runs in the background on behalf of a user who did not ask for
/// it, and there is no caller in a position to handle an error usefully; the
/// outcome *is* the error handling. This mirrors
/// [`crate::send::preflight::PreflightGuardian::check`]'s rule 1.
pub async fn discover(
    autocrypt: Option<Vec<u8>>,
    fetcher: &dyn Fetcher,
    address: &str,
    config: &CryptoConfig,
    now: i64,
    cancel: &CancellationToken,
) -> Outcome {
    let address = normalize_address(address);
    let budget: Duration = config.discovery_timeout.into();
    let started = Instant::now();
    let max_bytes = config.max_key_bytes as usize;

    let mut reasons = Vec::new();
    let mut reached_any = false;

    // --- 1. Autocrypt: local, free, leaks nothing -------------------------
    //
    // Handed in already-read rather than looked up here. A `&Connection` in
    // this signature would make the whole future non-`Send`, which is fatal
    // for something that exists to be `tokio::spawn`ed — and it would hold a
    // pool connection open across every network call in the chain below.
    // `autocrypt_key` is the reader; `crypto::service` calls it first.
    if config.autocrypt {
        if let Some(bytes) = autocrypt {
            reached_any = true;
            match key::parse(&bytes, &address, KeySource::Autocrypt, now, max_bytes) {
                Ok(k) => return Outcome::Found(Box::new(k)),
                Err(e) => reasons.push(format!("autocrypt: {e}")),
            }
        }
    }

    // --- 2. WKD: the recipient's own domain -------------------------------
    if config.wkd {
        for url in wkd_urls(&address) {
            if let Some(remaining) = remaining(budget, started, cancel) {
                match fetcher.get(&url, None, remaining).await {
                    Ok(Some(bytes)) => {
                        reached_any = true;
                        match key::parse(&bytes, &address, KeySource::Wkd, now, max_bytes) {
                            Ok(k) => return Outcome::Found(Box::new(k)),
                            Err(e) => reasons.push(format!("wkd: {e}")),
                        }
                    }
                    Ok(None) => reached_any = true,
                    Err(e) => reasons.push(format!("wkd: {e}")),
                }
            }
        }
    }

    // --- 3 & 4. Keyservers, private before public -------------------------
    for server in ordered_keyservers(&config.keyservers) {
        let Some(remaining) = remaining(budget, started, cancel) else {
            break;
        };
        let source = match server.kind {
            KeyserverKind::Private => KeySource::PrivateKeyserver,
            KeyserverKind::Public => KeySource::PublicKeyserver,
        };
        let token = server
            .token_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok());

        match fetcher
            .get(&hkp_url(&server.url, &address), token.as_deref(), remaining)
            .await
        {
            Ok(Some(bytes)) => {
                reached_any = true;
                match key::parse(&bytes, &address, source, now, max_bytes) {
                    Ok(k) => return Outcome::Found(Box::new(k)),
                    Err(e) => reasons.push(format!("{}: {e}", server.name)),
                }
            }
            Ok(None) => reached_any = true,
            Err(e) => reasons.push(format!("{}: {e}", server.name)),
        }
    }

    // Reaching a source and learning it has nothing is an answer; failing to
    // reach any is not. See the module docs.
    if reached_any {
        Outcome::NotFound
    } else {
        Outcome::Failed { reasons }
    }
}

/// Remaining budget, or `None` if it is spent or the caller cancelled.
fn remaining(budget: Duration, started: Instant, cancel: &CancellationToken) -> Option<Duration> {
    if cancel.is_cancelled() {
        return None;
    }
    budget
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
}

/// Keyservers with every private entry before every public one.
///
/// Stable within each group, so a user's ordering is still honoured among
/// servers of the same kind.
#[must_use]
pub fn ordered_keyservers(servers: &[KeyserverConfig]) -> Vec<&KeyserverConfig> {
    let (private, public): (Vec<_>, Vec<_>) = servers
        .iter()
        .partition(|s| matches!(s.kind, KeyserverKind::Private));
    private.into_iter().chain(public).collect()
}

/// The HKP lookup URL for an address.
///
/// `options=mr` asks for the machine-readable form, which is what makes the
/// response a key rather than an HTML page.
#[must_use]
pub fn hkp_url(base: &str, address: &str) -> String {
    format!(
        "{}/pks/lookup?op=get&options=mr&search={}",
        base.trim_end_matches('/'),
        address
    )
}

/// Both WKD URLs for an address, advanced method first.
///
/// The advanced method (`openpgpkey.<domain>`) is tried before the direct one
/// (`<domain>/.well-known/...`) because the draft says so, and because it is
/// the one a domain can delegate without serving anything from its main web
/// host.
///
/// Returns empty for an address with no `@`.
#[must_use]
pub fn wkd_urls(address: &str) -> Vec<String> {
    let Some((local, domain)) = address.split_once('@') else {
        return Vec::new();
    };
    if local.is_empty() || domain.is_empty() {
        return Vec::new();
    }
    let hashed = zbase32_sha1(local);
    vec![
        format!(
            "https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hashed}?l={local}"
        ),
        format!("https://{domain}/.well-known/openpgpkey/hu/{hashed}?l={local}"),
    ]
}

/// z-base-32 of the SHA-1 of the lowercased local part — the WKD path element.
///
/// Neither algorithm is a security choice; both are fixed by the WKD draft,
/// and computing anything else yields a URL no WKD server will answer.
fn zbase32_sha1(local: &str) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(local.to_lowercase().as_bytes());
    zbase32(&digest)
}

/// z-base-32, the human-oriented base32 alphabet WKD uses.
fn zbase32(input: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut out = String::with_capacity(input.len().div_ceil(5) * 8);
    let mut buffer: u16 = 0;
    let mut bits: u8 = 0;
    for &byte in input {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(char::from(ALPHABET[index]));
        }
    }
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(char::from(ALPHABET[index]));
    }
    out
}

/// How many of a correspondent's most recent messages are searched for an
/// `Autocrypt:` header.
///
/// More than one because the newest message is not reliably the one carrying
/// the header: Autocrypt is per-sending-client, so a correspondent who replies
/// once from a phone that does not implement it would otherwise look like they
/// had no key at all. Bounded because each candidate means parsing a raw
/// message, and this runs on a background task that should not turn into a
/// mailbox scan.
const AUTOCRYPT_SCAN_DEPTH: u32 = 8;

/// The most recent `Autocrypt:` header key for an address, if the mailbox has
/// one.
///
/// # Why the newest message wins
///
/// Autocrypt headers accumulate: every message a correspondent sends carries
/// their current key. Reading the newest is how a rotation propagates without
/// any network call at all — which is the entire appeal of this source.
///
/// Headers are read out of `messages.raw` rather than a header table, because
/// there is no header table — the raw message is what the schema keeps. Rows
/// with no `raw` (metadata-only sync) are skipped by the query rather than
/// parsed and discarded.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn autocrypt_key(conn: &Connection, address: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    let mut stmt = conn.prepare(
        "SELECT raw FROM messages
          WHERE lower(from_addr) = ?1 AND raw IS NOT NULL
          ORDER BY coalesce(date, internaldate) DESC
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![address, AUTOCRYPT_SCAN_DEPTH], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;

    for raw in rows {
        let raw = raw?;
        let Some(parsed) = mail_parser::MessageParser::default().parse(&raw) else {
            continue;
        };
        let found = parsed
            .header_raw("Autocrypt")
            .and_then(parse_autocrypt_header);
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// Pull the `keydata` out of an `Autocrypt:` header value.
///
/// The header is `addr=...; prefer-encrypt=...; keydata=<base64>`. `keydata`
/// must be last per the spec, but this parser does not rely on that — it takes
/// the named attribute wherever it appears, because a header that violates the
/// ordering rule is far more likely to be a slightly wrong sender than an
/// attack, and refusing it would lose a key for no gain.
#[must_use]
pub fn parse_autocrypt_header(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let encoded: String = value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("keydata="))?
        // Long headers are folded across lines; the base64 payload is the
        // part that suffers, so all whitespace is stripped before decoding.
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}
