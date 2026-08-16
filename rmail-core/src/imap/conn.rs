//! IMAP connection establishment, login, and capability probing.

use std::fmt::Debug;
use std::sync::Arc;

use async_imap::{Client, Session};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{map_imap_err, ImapCapabilities};
use crate::error::Error;

/// The concrete TLS client stream type (async-imap `runtime-tokio` drives tokio
/// streams directly, so no compatibility wrapper is needed).
pub type TlsClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Trait bundle for a stream async-imap can drive.
pub trait ImapStream: AsyncRead + AsyncWrite + Unpin + Debug + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Debug + Send> ImapStream for T {}

/// Connect to `host:port` over `rustls` TLS, returning a stream ready for
/// [`login`].
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an unparsable host; [`Error::Unavailable`] if
/// the host is unreachable or the TLS handshake fails.
pub async fn connect_tls(host: &str, port: u16) -> Result<TlsClientStream, Error> {
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    // Before the first handshake, and idempotent: rustls otherwise infers a
    // provider from crate features and panics when more than one is present.
    crate::transport::install_crypto_provider();

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| Error::invalid_argument(format!("invalid IMAP server name: {host}")))?;

    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| Error::unavailable(format!("cannot reach IMAP server {host}:{port}: {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::unavailable(format!("IMAP TLS handshake failed: {e}")))?;
    Ok(tls)
}

/// Connect to an account's IMAP server, log in, and probe capabilities.
///
/// The one place credentials are resolved for a sync: [`crate::imap::test_connection`]
/// does the same handshake but exists to *verify* configuration and logs out
/// afterwards. This hands the live session back so the caller can work with it.
///
/// The credential is resolved on a blocking thread — a `password_command` or a
/// keychain prompt can block for a long time — and never leaves this scope.
///
/// # Errors
///
/// - [`Error::FailedPrecondition`] if the account has no IMAP server or no
///   credential configured.
/// - [`Error::NotFound`] if the account does not exist.
/// - [`Error::Unauthenticated`] if the server rejects the login.
/// - [`Error::Unavailable`] if the server is unreachable.
/// - [`Error::DeadlineExceeded`] if the handshake exceeds
///   [`crate::imap::IMAP_DEADLINE`].
#[tracing::instrument(skip(db), err)]
pub async fn connect_account(
    db: &crate::storage::Database,
    account_id: i64,
) -> Result<(Session<TlsClientStream>, super::ImapCapabilities), Error> {
    let account = crate::account::get(db, account_id).await?;
    let host = account
        .imap_server
        .clone()
        .ok_or_else(|| Error::failed_precondition("account has no IMAP server configured"))?;
    let port = account.imap_port.unwrap_or(993);
    let username = account.username.clone().unwrap_or_default();

    // An OAuth account has no password to resolve; it presents a bearer token
    // over SASL XOAUTH2 instead. The token is fetched *before* the deadline
    // starts because obtaining it may involve a network refresh and a Keychain
    // read, neither of which is part of the IMAP handshake this deadline
    // bounds — and a refresh that pushed the handshake over the limit would
    // surface as "IMAP connection timed out" rather than as the token error it
    // is.
    let oauth = account.credential.is_oauth();
    let secret = if oauth {
        let key = crate::oauth::key_for(&account)?;
        crate::oauth::broker()?.access_token(&key).await?
    } else {
        let credential = account.credential.clone();
        let username_for_resolve = account.username.clone();
        tokio::task::spawn_blocking(move || credential.resolve(username_for_resolve.as_deref()))
            .await
            .map_err(|e| Error::internal(format!("credential resolution task failed: {e}")))??
            .ok_or_else(|| Error::failed_precondition("account has no credential configured"))?
    };

    // Bound the handshake so a wedged server cannot hang a sync before it has
    // even started.
    tokio::time::timeout(super::IMAP_DEADLINE, async {
        let stream = connect_tls(&host, port).await?;
        let mut session = if oauth {
            authenticate_xoauth2(stream, &username, &secret).await?
        } else {
            login(stream, &username, secret.expose()).await?
        };
        let capabilities = probe_capabilities(&mut session).await?;
        Ok((session, capabilities))
    })
    .await
    .map_err(|_| Error::deadline_exceeded("IMAP connection timed out"))?
}

/// Authenticate with SASL `XOAUTH2` (RFC 6749 bearer tokens over IMAP), the
/// mechanism Google and Microsoft require for OAuth accounts.
///
/// `async-imap` base64-encodes whatever the authenticator returns, so the
/// authenticator hands back the raw SASL string [`crate::oauth::xoauth2`]
/// builds.
///
/// # Errors
///
/// [`Error::Unavailable`] if the greeting is missing/broken;
/// [`Error::Unauthenticated`] if the server rejects the token.
pub async fn authenticate_xoauth2<T: ImapStream>(
    stream: T,
    username: &str,
    access_token: &crate::Secret,
) -> Result<Session<T>, Error> {
    let mut client = Client::new(stream);
    read_greeting(&mut client).await?;
    client
        .authenticate(
            "XOAUTH2",
            Xoauth2 {
                response: crate::oauth::xoauth2(username, access_token.expose()),
            },
        )
        .await
        .map_err(|(err, _client)| {
            // A rejected bearer token is `Unauthenticated` by way of
            // `map_imap_err`, which is the code that tells the caller to
            // re-authorize rather than to retry — the same conclusion the
            // broker reaches for a revoked refresh token.
            map_imap_err(err)
        })
}

/// The SASL exchange for XOAUTH2: one response, ignoring the (empty) server
/// challenge.
///
/// On failure Google and Microsoft send a *second* continuation carrying a
/// base64 JSON error rather than tagging the command `NO`; the client is
/// expected to answer with an empty line, which is what the empty response
/// after the first challenge produces. Returning the credential again there
/// would replay the token into a channel the server has already declared
/// failed.
struct Xoauth2 {
    response: crate::Secret,
}

impl async_imap::Authenticator for Xoauth2 {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        let out = self.response.expose().as_bytes().to_vec();
        // One shot: any further challenge is the server's error report, and
        // the protocol wants an empty line back to close the exchange.
        self.response = crate::Secret::new(String::new());
        out
    }
}

/// Read and discard the untagged server greeting.
async fn read_greeting<T: ImapStream>(client: &mut Client<T>) -> Result<(), Error> {
    match client.read_response().await {
        Some(Ok(_greeting)) => Ok(()),
        Some(Err(e)) => Err(Error::unavailable(format!("IMAP greeting failed: {e}"))),
        None => Err(Error::unavailable(
            "IMAP server closed before sending a greeting",
        )),
    }
}

/// Read the server greeting and log in, returning an authenticated session.
///
/// Generic over the stream so production (TLS) and tests (plain TCP) share the
/// path.
///
/// # Errors
///
/// [`Error::Unavailable`] if the greeting is missing/broken; [`Error::Unauthenticated`]
/// if the server rejects the login.
pub async fn login<T: ImapStream>(
    stream: T,
    username: &str,
    password: &str,
) -> Result<Session<T>, Error> {
    let mut client = Client::new(stream);
    read_greeting(&mut client).await?;
    client
        .login(username, password)
        .await
        .map_err(|(err, _client)| map_imap_err(err))
}

/// Probe the capabilities rmail cares about.
///
/// # Errors
///
/// [`Error::Unavailable`] if the `CAPABILITY` command fails.
pub async fn probe_capabilities<T: ImapStream>(
    session: &mut Session<T>,
) -> Result<ImapCapabilities, Error> {
    let caps = session.capabilities().await.map_err(map_imap_err)?;
    Ok(ImapCapabilities {
        idle: caps.has_str("IDLE"),
        condstore: caps.has_str("CONDSTORE"),
        qresync: caps.has_str("QRESYNC"),
        move_: caps.has_str("MOVE"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::mock::{MockConfig, MockImap};
    use crate::ErrorReason;

    async fn connect_mock(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
        tokio::net::TcpStream::connect(addr).await.unwrap()
    }

    #[tokio::test]
    async fn login_succeeds_with_correct_password() {
        let mock = MockImap::start(MockConfig::default().password("hunter2")).await;
        let stream = connect_mock(mock.addr).await;
        let session = login(stream, "user@example.com", "hunter2").await;
        assert!(session.is_ok(), "login should succeed: {:?}", session.err());
    }

    #[tokio::test]
    async fn login_rejected_is_unauthenticated() {
        let mock = MockImap::start(MockConfig::default().password("hunter2")).await;
        let stream = connect_mock(mock.addr).await;
        let err = login(stream, "user@example.com", "wrong")
            .await
            .expect_err("bad password must be rejected");
        assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    }

    #[tokio::test]
    async fn capability_probe_reads_flags() {
        let mock = MockImap::start(MockConfig::default().password("pw")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();
        let caps = probe_capabilities(&mut session).await.unwrap();
        assert!(caps.idle, "mock advertises IDLE");
        assert!(caps.condstore);
        assert!(caps.qresync);
        assert!(caps.move_);
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn xoauth2_authenticates_with_the_bearer_token() {
        let mock = MockImap::start(MockConfig::default().xoauth2("ya29.good-token")).await;
        let stream = connect_mock(mock.addr).await;
        let session = authenticate_xoauth2(
            stream,
            "user@example.com",
            &crate::Secret::new("ya29.good-token"),
        )
        .await;
        assert!(
            session.is_ok(),
            "XOAUTH2 should succeed: {:?}",
            session.err()
        );
        // The server saw the exact SASL string, base64-decoded.
        let sasl = mock
            .commands()
            .into_iter()
            .find(|c| c.starts_with("SASL "))
            .expect("the mock records the decoded SASL response");
        assert_eq!(
            sasl,
            "SASL user=user@example.com\u{1}auth=Bearer ya29.good-token\u{1}\u{1}"
        );
    }

    /// A rejected bearer token must come back as `Unauthenticated` — and must
    /// not hang. Real servers answer a bad token with a *second* continuation
    /// rather than a tagged `NO`, so a client that replies to it with the
    /// credential again never terminates the exchange.
    #[tokio::test]
    async fn xoauth2_with_a_stale_token_is_unauthenticated() {
        let mock = MockImap::start(MockConfig::default().xoauth2("ya29.good-token")).await;
        let stream = connect_mock(mock.addr).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            authenticate_xoauth2(
                stream,
                "user@example.com",
                &crate::Secret::new("ya29.stale"),
            ),
        )
        .await
        .expect("a rejected token must not hang the exchange");
        let err = result.expect_err("a stale token must be rejected");
        assert_eq!(err.reason(), ErrorReason::Unauthenticated);
        assert!(
            !err.to_string().contains("ya29.stale"),
            "the token must not appear in the error: {err}"
        );
    }

    #[tokio::test]
    async fn unreachable_host_is_unavailable() {
        // Port 1 on loopback refuses fast — exercises the TCP-connect error path
        // without any network.
        let err = connect_tls("127.0.0.1", 1)
            .await
            .expect_err("connection should be refused");
        assert_eq!(err.reason(), ErrorReason::Unavailable);
    }
}
