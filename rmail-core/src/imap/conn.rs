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
    match client.read_response().await {
        Some(Ok(_greeting)) => {}
        Some(Err(e)) => return Err(Error::unavailable(format!("IMAP greeting failed: {e}"))),
        None => {
            return Err(Error::unavailable(
                "IMAP server closed before sending a greeting",
            ))
        }
    }
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
    async fn unreachable_host_is_unavailable() {
        // Port 1 on loopback refuses fast — exercises the TCP-connect error path
        // without any network.
        let err = connect_tls("127.0.0.1", 1)
            .await
            .expect_err("connection should be refused");
        assert_eq!(err.reason(), ErrorReason::Unavailable);
    }
}
