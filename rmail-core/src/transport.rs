//! Runtime socket conventions and the shared Unix-domain-socket gRPC connector.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

/// Environment variable naming the rmaild Unix domain socket.
pub const SOCKET_ENV: &str = "RMAIL_SOCKET";

/// Default socket path when [`SOCKET_ENV`] is unset.
///
/// Prefers `$HOME/.local/state/rmail/rmaild.sock`, falling back to a path under
/// the system temp directory when `$HOME` is unavailable.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("rmail")
            .join("rmaild.sock"),
        None => std::env::temp_dir().join("rmail").join("rmaild.sock"),
    }
}

/// Socket path resolved from [`SOCKET_ENV`], falling back to
/// [`default_socket_path`].
#[must_use]
pub fn socket_path_from_env() -> PathBuf {
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

/// Connection-establishment timeout applied to [`connect_uds`]. A per-request
/// deadline is the caller's responsibility (streaming RPCs must not inherit a
/// blanket channel timeout).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Establish a gRPC [`Channel`] to rmaild over its Unix domain socket.
///
/// The HTTP authority is a placeholder — the custom connector dials the socket
/// directly, ignoring the URI host/port. A [`CONNECT_TIMEOUT`] bounds connection
/// establishment so a wedged daemon cannot hang the caller indefinitely.
///
/// # Errors
///
/// Returns a [`tonic::transport::Error`] if the endpoint cannot be constructed
/// or the socket connection fails.
pub async fn connect_uds(path: impl AsRef<Path>) -> Result<Channel, tonic::transport::Error> {
    let path = path.as_ref().to_path_buf();
    let endpoint = Endpoint::try_from("http://[::1]:50051")?.connect_timeout(CONNECT_TIMEOUT);
    endpoint
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
}
