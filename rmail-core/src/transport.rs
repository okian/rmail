//! Runtime socket conventions and the shared Unix-domain-socket gRPC connector.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};

/// Environment variable naming the rmaild Unix domain socket.
pub const SOCKET_ENV: &str = "RMAIL_SOCKET";

/// Environment variable naming the rmail SQLite database file.
pub const DB_ENV: &str = "RMAIL_DB";

/// Environment variable naming the master TOML config file.
pub const CONFIG_ENV: &str = "RMAIL_CONFIG";

/// Default config path when [`CONFIG_ENV`] is unset.
#[must_use]
pub fn default_config_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("rmail")
            .join("config.toml"),
        None => std::env::temp_dir().join("rmail").join("config.toml"),
    }
}

/// Config path resolved from [`CONFIG_ENV`], falling back to
/// [`default_config_path`].
#[must_use]
pub fn config_path_from_env() -> PathBuf {
    std::env::var_os(CONFIG_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path)
}

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

/// Default database path when [`DB_ENV`] is unset: `<data dir>/rmail.db`,
/// sibling to the default socket.
#[must_use]
pub fn default_db_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("rmail")
            .join("rmail.db"),
        None => std::env::temp_dir().join("rmail").join("rmail.db"),
    }
}

/// Database path resolved from [`DB_ENV`], falling back to [`default_db_path`].
#[must_use]
pub fn db_path_from_env() -> PathBuf {
    std::env::var_os(DB_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_db_path)
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

/// Install the process-wide rustls crypto provider.
///
/// # Why this is explicit
///
/// rustls picks a provider from *crate features* when none is installed, and
/// that inference fails as soon as anything in the dependency graph enables a
/// second one — which happened here the moment an HTTP client was added
/// alongside the IMAP client. The failure is a panic on the first handshake, at
/// runtime, in whichever code path happens to reach TLS first. Choosing the
/// provider in one place removes a whole class of dependency-ordering surprise
/// and makes the choice reviewable.
///
/// Idempotent and safe to call from anywhere: a second call finds a provider
/// already installed and leaves it alone, because whatever is installed is
/// already serving live connections.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            tracing::debug!("a rustls crypto provider was already installed");
        }
    });
}

#[cfg(test)]
mod crypto_tests {
    use super::install_crypto_provider;

    #[test]
    fn installing_the_provider_twice_is_harmless() {
        // Called from every TLS entry point rather than from one startup path,
        // precisely so no future entry point can forget. That is only safe if
        // repeating it cannot fail — and the second call must leave the
        // installed provider alone, because it is already serving connections.
        install_crypto_provider();
        install_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "a provider must be installed once anything has asked for one"
        );
    }
}
