//! The loopback redirect listener (RFC 8252 §7.3).
//!
//! A one-shot HTTP server on `127.0.0.1:<ephemeral>` that the provider
//! redirects the user's browser to. It speaks the smallest amount of HTTP that
//! a browser will accept, because the alternative is linking a web server into
//! a mail daemon to serve exactly one response.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::credential::Secret;
use crate::error::Error;

use super::pkce::random_state;
use super::url::parse_query;

/// The path the provider is told to redirect to.
///
/// A path rather than bare `/` so that a stray request to the root — which a
/// browser will make if the user opens the port by hand — is answered with a
/// 404 instead of being parsed as a callback with no code.
const CALLBACK_PATH: &str = "/rmail/oauth/callback";

/// How long the flow waits for the user to finish consenting.
///
/// Long enough to find the browser window, log in, and pick an account from a
/// chooser; short enough that a forgotten flow releases its port and its
/// verifier rather than living for the life of the daemon.
pub const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest request head the listener will read before giving up.
///
/// The peer is a browser on loopback, but the socket is reachable by any local
/// process, so an unbounded read is an unbounded allocation any user on the
/// machine can drive.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// How long a single misbehaving local client may hold the listener.
///
/// Without it, a process that connects and sends nothing pins the flow until
/// [`AUTHORIZATION_TIMEOUT`] — a trivial denial of service against the one
/// socket the browser needs to reach.
const PER_CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// A bound loopback listener awaiting one authorization redirect.
#[derive(Debug)]
pub struct LoopbackRedirect {
    listener: TcpListener,
    redirect_uri: String,
    state: Secret,
}

impl LoopbackRedirect {
    /// Bind an ephemeral loopback port.
    ///
    /// Binding happens when the flow *starts*, not when the redirect arrives,
    /// so the port in the authorization URL is one this process already holds:
    /// a URL naming a port bound later is a URL another local process can
    /// bind first and receive the code on.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] if no loopback port can be bound.
    pub async fn bind() -> Result<Self, Error> {
        // 127.0.0.1 rather than `localhost`: the latter goes through the
        // host's resolver, which /etc/hosts or a DNS search domain can point
        // somewhere else. RFC 8252 §8.3 says the same.
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| Error::unavailable(format!("could not bind a loopback redirect: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| Error::unavailable(format!("could not read the redirect port: {e}")))?;
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{}{CALLBACK_PATH}", addr.port()),
            state: random_state(),
        })
    }

    /// The redirect URI to register in the authorization request.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The `state` this flow expects back.
    #[must_use]
    pub fn state(&self) -> &Secret {
        &self.state
    }

    /// Serve requests until the authorization redirect arrives, and return the
    /// code it carried.
    ///
    /// Anything that is not the callback (a browser's `/favicon.ico`, a port
    /// scanner, a user opening the root by hand) is answered and ignored
    /// rather than ending the flow — a favicon fetch racing the redirect would
    /// otherwise fail every authorization done in a browser that prefetches.
    ///
    /// # Errors
    ///
    /// - [`Error::PermissionDenied`] if the user declined consent.
    /// - [`Error::Unauthenticated`] if the redirect carried no code. A
    ///   redirect carrying an *unrecognized* `state` is refused without ending
    ///   the flow — see [`LoopbackRedirect::serve_one`].
    /// - [`Error::DeadlineExceeded`] after [`AUTHORIZATION_TIMEOUT`].
    /// - [`Error::Cancelled`] if `cancel` fires first.
    pub async fn wait_for_code(&self, cancel: CancellationToken) -> Result<Secret, Error> {
        let accepted = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(Error::cancelled("the OAuth authorization was cancelled"));
            }
            result = tokio::time::timeout(AUTHORIZATION_TIMEOUT, self.accept_loop()) => result,
        };
        accepted.map_err(|_| {
            Error::deadline_exceeded(
                "no authorization redirect arrived; the browser was never opened, \
                 or consent was not completed in time",
            )
        })?
    }

    async fn accept_loop(&self) -> Result<Secret, Error> {
        loop {
            let (stream, _peer) =
                self.listener.accept().await.map_err(|e| {
                    Error::unavailable(format!("the redirect listener failed: {e}"))
                })?;
            // A slow or silent client must not consume the whole authorization
            // window, so each connection is separately bounded and a timeout
            // simply moves on to the next one.
            match tokio::time::timeout(PER_CONNECTION_TIMEOUT, self.serve_one(stream)).await {
                Ok(Some(outcome)) => return outcome,
                Ok(None) => continue,
                Err(_) => {
                    tracing::debug!("a connection to the OAuth redirect timed out; still waiting");
                    continue;
                }
            }
        }
    }

    /// Handle one connection. `None` means "not the callback, keep waiting".
    async fn serve_one(&self, mut stream: TcpStream) -> Option<Result<Secret, Error>> {
        let request = read_request_line(&mut stream).await?;
        let Some(query) = request.strip_prefix(CALLBACK_PATH) else {
            respond(&mut stream, "404 Not Found", "Not the rmail callback.").await;
            return None;
        };
        // `?...` or nothing; a bare `/rmail/oauth/callback` has no parameters
        // and is as invalid as a foreign request.
        let params = parse_query(query.strip_prefix('?').unwrap_or(""));
        let get = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };

        // `state` is checked before anything else is even read, so a foreign
        // redirect cannot get as far as having its `code` inspected.
        //
        // A mismatch refuses *that request* and keeps waiting, rather than
        // ending the flow. Ending it would hand any local process a one-packet
        // denial of service against every authorization this daemon starts —
        // the port is enumerable, and killing the flow needs no secret at all.
        // Continuing costs nothing: no code is accepted without the right
        // state either way, and the genuine redirect still lands.
        let presented = get("state").unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), self.state.expose().as_bytes()) {
            respond(
                &mut stream,
                "400 Bad Request",
                "This redirect did not come from the authorization rmail started.",
            )
            .await;
            // Not logged with the presented value: `state` is a bearer
            // credential for whichever flow *did* issue it.
            tracing::warn!("refused an OAuth redirect whose state did not match; still waiting");
            return None;
        }

        if let Some(error) = get("error") {
            respond(
                &mut stream,
                "200 OK",
                "Authorization was declined. You can close this window.",
            )
            .await;
            // The error *code* is a fixed vocabulary (RFC 6749 §4.1.2.1) and
            // safe to repeat; `error_description` is provider free text and is
            // deliberately dropped.
            return Some(Err(if error == "access_denied" {
                Error::permission_denied("authorization was declined")
            } else {
                Error::unauthenticated(format!("the provider refused the authorization: {error}"))
            }));
        }

        match get("code").filter(|code| !code.is_empty()) {
            Some(code) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "rmail is authorized. You can close this window.",
                )
                .await;
                Some(Ok(Secret::new(code)))
            }
            None => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "The redirect carried no authorization code.",
                )
                .await;
                Some(Err(Error::unauthenticated(
                    "the authorization redirect carried no code",
                )))
            }
        }
    }
}

/// Read the request target out of an HTTP request line.
///
/// Returns `None` for anything that is not a `GET` of a path — including a
/// connection that sends nothing, which is what a port scanner looks like.
async fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let read = stream.read(&mut buf).await.ok()?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if raw.windows(2).any(|w| w == b"\r\n") || raw.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    Some(parts.next()?.to_owned())
}

/// Write a minimal HTML response. Failures are ignored: the browser having
/// hung up does not change whether the code arrived.
async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>rmail</title>\
         <body style=\"font:16px system-ui;padding:3rem\"><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// Compare two byte strings without an early exit on the first difference.
///
/// `state` is a secret the caller is trying to guess; a `==` that returns as
/// soon as two bytes differ leaks how much of the guess was right, and the
/// attacker here is a local process that can retry as fast as it likes.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
