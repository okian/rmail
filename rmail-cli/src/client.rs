//! Where every RPC this binary makes gets its channel (task 42).
//!
//! # One connector, not forty-one
//!
//! prd.md gives `mail` six global transport flags — `--socket`, `--addr`,
//! `--token`, `--tls-ca/--tls-cert/--tls-key`, `--insecure`, `--deadline` —
//! and the only way for those to be true of *every* verb is for every verb to
//! reach the daemon through one function. Before this module each command
//! called `rmail_core::connect_uds` itself, which made `--socket` real and the
//! other five unimplementable without editing ninety functions.
//!
//! So the call sites now say [`connect`], and the flags live here.
//!
//! # A `OnceLock`, and why that is not hidden state
//!
//! The transport is fixed by argv before any command runs and never changes
//! afterwards. Threading it through ninety signatures would express that fact
//! by repeating it ninety times, and every one of those functions takes the
//! socket path today only to hand it straight to the connector. [`init`] is
//! called exactly once, from `main`, and nothing writes it again — the
//! `OnceLock` is what enforces that rather than a convention. A caller that
//! reads it first (a unit test) gets [`Transport::default`], which is the
//! Unix socket at `$RMAIL_SOCKET`: the same behaviour `mail` had before this
//! module existed.
//!
//! # Auth is the daemon's job, and the token does not bypass it
//!
//! `--token` attaches `authorization: Bearer …` to every request. It grants
//! nothing by itself: `rmaild`'s `AuthLayer` verifies it against the token
//! table and checks the per-method scope requirement, and over a Unix socket
//! the peer-uid check is consulted *first*, so a narrow token presented by the
//! daemon's own user is not a way to test a denial (it is simply ignored —
//! see `rmaild::auth`'s "Two principals"). Nothing in this module can widen
//! what a principal may do; it can only carry the credential.
//!
//! # The client_auth session cache is a fallback, not a third flag
//!
//! When neither `--token` nor `$RMAIL_TOKEN` is given (and `--addr` is not
//! in play — see [`connect_parts`]), the token comes from whatever `mail
//! auth login` cached for this exact socket path ([`crate::session`]). This
//! exists so `client_auth.require_for_local` is actually usable day to day:
//! without it, a daemon with that flag on would need `--token` typed by hand
//! on every single command forever, which is a strong incentive to just turn
//! the flag back off. The cache carries no more authority than an explicit
//! `--token` would — it is still just a bearer secret `rmaild`'s `AuthLayer`
//! verifies the same way.
//!
//! # Nothing hangs
//!
//! `rmail_core::CONNECT_TIMEOUT` bounds connection establishment, and a socket
//! path that does not exist fails immediately rather than blocking — so `mail`
//! run with no daemon reports [`crate::format::ExitCode::FailedPrecondition`]
//! in milliseconds, naming `mail daemon start`. That refusal is deliberate:
//! prd.md allows auto-start *or* a `FAILED_PRECONDITION`, and a CLI that
//! silently forks a long-lived daemon out of `mail search` is a surprise in
//! exactly the environments (CI, a container, an SSH one-liner) where a
//! surprise costs the most.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

use crate::format::{Classified, ExitCode};

/// The channel type every generated client in this crate is built on.
///
/// `InterceptedService` rather than a bare [`Channel`] because the bearer
/// token and the deadline have to be attached to *every* request, including
/// the ones inside streaming loops, and an interceptor is the only place that
/// is true without each call site remembering.
pub(crate) type Client = InterceptedService<Channel, Decorate>;

/// Where the daemon is and how to talk to it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Transport {
    /// `--addr host:port`. `None` means the Unix socket.
    addr: Option<String>,
    /// `--token`, or `$RMAIL_TOKEN`.
    token: Option<String>,
    /// `--deadline <secs>`, applied to every request as a gRPC deadline.
    deadline: Option<Duration>,
    /// `--tls-ca`: the root the server's certificate must chain to.
    tls_ca: Option<PathBuf>,
    /// `--tls-cert`/`--tls-key`: a client certificate, for mutual TLS.
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    /// `--insecure`: talk plaintext to `--addr`.
    insecure: bool,
}

static TRANSPORT: std::sync::OnceLock<Transport> = std::sync::OnceLock::new();

/// Record the transport flags. Called once, from `main`.
pub(crate) fn init(transport: Transport) {
    let _ = TRANSPORT.set(transport);
}

fn transport() -> Transport {
    TRANSPORT.get().cloned().unwrap_or_default()
}

/// The `--addr` this invocation was given, if any.
///
/// For the verbs that are inherently about the *local* daemon (`mail daemon
/// start|status|stop` spawns a process, reads a local pid file and sends a
/// local signal) and must refuse rather than silently answer about the Unix
/// socket.
pub(crate) fn remote_addr() -> Option<String> {
    transport().addr
}

/// The bearer secret `--token`/`$RMAIL_TOKEN` supplied, if any.
///
/// For `mail mcp serve`, which hands it to `rmaild::mcp::Principal` rather
/// than to a channel interceptor. It used to declare a `--token` of its own;
/// two arguments with one id are merged by `clap` rather than reported, so the
/// only safe arrangement is one declaration and one reader.
pub(crate) fn bearer() -> Option<String> {
    transport().token
}

impl Transport {
    /// Build the transport from the parsed global flags.
    ///
    /// # Errors
    ///
    /// If the flags contradict each other. Caught here rather than at connect
    /// time so `mail --tls-cert x search …` fails before it does any work,
    /// and with [`ExitCode::Usage`] rather than a transport error that reads
    /// like the daemon's fault.
    pub(crate) fn new(
        addr: Option<String>,
        token: Option<String>,
        deadline_secs: Option<u64>,
        tls_ca: Option<PathBuf>,
        tls_cert: Option<PathBuf>,
        tls_key: Option<PathBuf>,
        insecure: bool,
    ) -> Result<Self> {
        // `Classified`, not `bail!`: every one of these is a mistake in the
        // command line, and `ExitCode::of` has nothing to classify a bare
        // `anyhow!` by — it would exit 1, which is the "something failed" code
        // rather than the "you typed it wrong" one this doc comment promises.
        let usage = |message: &str| Classified::new(ExitCode::Usage, message.to_owned());
        if tls_cert.is_some() != tls_key.is_some() {
            return Err(usage(
                "--tls-cert and --tls-key must be given together: a client certificate is a pair",
            ));
        }
        let tls_requested = tls_ca.is_some() || tls_cert.is_some();
        if addr.is_none() && (tls_requested || insecure) {
            return Err(usage(
                "--tls-* and --insecure describe a TCP connection; give --addr <host:port> as \
                 well, or drop them to use the Unix socket",
            ));
        }
        if insecure && tls_requested {
            return Err(usage(
                "--insecure disables TLS, so it cannot be combined with --tls-ca/--tls-cert",
            ));
        }
        let deadline = match deadline_secs {
            // Zero is not "no deadline" spelled differently — it is a request
            // that has already expired, and reading it as "unlimited" is the
            // sort of off-by-one that only shows up in production.
            Some(0) => return Err(usage("--deadline must be a positive number of seconds")),
            Some(secs) => Some(Duration::from_secs(secs)),
            None => None,
        };
        Ok(Self {
            addr,
            token,
            deadline,
            tls_ca,
            tls_cert,
            tls_key,
            insecure,
        })
    }

    /// How this transport describes itself in an error message. Never
    /// includes the token.
    fn describe(&self, socket: &Path) -> String {
        match &self.addr {
            Some(addr) => format!("rmaild at {addr}"),
            None => format!("rmaild at {}", socket.display()),
        }
    }
}

/// Attaches the bearer token and the deadline to every outgoing request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Decorate {
    token: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
    deadline: Option<Duration>,
}

impl tonic::service::Interceptor for Decorate {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.token {
            request
                .metadata_mut()
                .insert("authorization", token.clone());
        }
        if let Some(deadline) = self.deadline {
            // A gRPC deadline, not a local timeout: the header travels, so the
            // daemon stops working on a request nobody is waiting for instead
            // of finishing it into a closed stream.
            request.set_timeout(deadline);
        }
        Ok(request)
    }
}

/// A connection before the interceptor is wrapped around it.
///
/// `mail api call` needs these separately: it dispatches through
/// `rmaild::mcp::invoke::call_dynamic`, which builds its own request over a
/// bare [`Channel`] and takes the bearer and the deadline as arguments — so
/// handing it an already-intercepted service would put two `authorization`
/// headers on the wire.
pub(crate) struct Parts {
    /// The connected channel.
    pub(crate) channel: Channel,
    /// `--token`/`$RMAIL_TOKEN`, unparsed.
    pub(crate) token: Option<String>,
    /// `--deadline`, if one was given.
    pub(crate) deadline: Option<Duration>,
}

/// Connect to the daemon, honouring every global transport flag.
///
/// `socket` is the path `--socket`/`$RMAIL_SOCKET` resolved to, and is used
/// unless `--addr` named a TCP endpoint instead.
///
/// # Errors
///
/// [`ExitCode::FailedPrecondition`] when the Unix socket is not there at all
/// — the daemon is not running, and the message says how to start it;
/// [`ExitCode::Unavailable`] when something is listening and the connection
/// still failed; [`ExitCode::InvalidArgument`] for a malformed `--addr` or an
/// unreadable certificate.
pub(crate) async fn connect_parts(socket: &Path) -> Result<Parts> {
    let transport = transport();
    let channel = match &transport.addr {
        Some(addr) => connect_tcp(&transport, addr).await?,
        None => connect_socket(&transport, socket).await?,
    };
    // `--token`/`$RMAIL_TOKEN` win outright; otherwise fall back to whatever
    // `mail auth login` cached for *this* socket — only for the Unix-socket
    // path, since the cache is keyed by socket path (see
    // `session::account_for`) and a `--addr` target has no such path to key
    // on. A `client_auth.require_for_local`-gated daemon depends on exactly
    // this: without it, every command after `mail auth login` would need
    // `--token` typed by hand, defeating the point of caching anything.
    let token = match &transport.token {
        Some(token) => Some(token.clone()),
        None if transport.addr.is_none() => {
            // `session::load` is a blocking Keychain call (the macOS
            // Keychain API is synchronous FFI, and on an unsigned or
            // rebuilt binary it can raise an OS access prompt) — run off
            // the async runtime like any other blocking I/O, so it cannot
            // stall this task's executor thread while a human is looking at
            // a dialog. Every command pays this, including ones talking to
            // a daemon with no password configured at all, which is exactly
            // why it must never block inline.
            let socket = socket.to_path_buf();
            tokio::task::spawn_blocking(move || crate::session::load(&socket))
                .await
                .unwrap_or_default()
                .map(|cached| cached.token)
        }
        None => None,
    };
    Ok(Parts {
        channel,
        token,
        deadline: transport.deadline,
    })
}

impl Parts {
    /// Wrap the channel so every request carries the token and the deadline.
    ///
    /// # Errors
    ///
    /// [`ExitCode::InvalidArgument`] if the token cannot be an HTTP header
    /// value.
    pub(crate) fn into_client(self) -> Result<Client> {
        let token = match &self.token {
            Some(secret) => Some(
                format!("Bearer {secret}")
                    .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
                    // Deliberately does not echo the value: it is a secret,
                    // and this message reaches stderr, logs and CI
                    // transcripts.
                    .map_err(|_| {
                        Classified::new(
                            ExitCode::InvalidArgument,
                            "the value of --token/$RMAIL_TOKEN contains characters that cannot go \
                             in an HTTP header",
                        )
                    })?,
            ),
            None => None,
        };
        Ok(InterceptedService::new(
            self.channel,
            Decorate {
                token,
                deadline: self.deadline,
            },
        ))
    }
}

/// Connect, ready for a generated client.
///
/// # Errors
///
/// As [`connect_parts`] and [`Parts::into_client`].
pub(crate) async fn connect(socket: &Path) -> Result<Client> {
    connect_parts(socket).await?.into_client()
}

/// The Unix-socket path: the default, and the only one `rmaild` serves today.
async fn connect_socket(transport: &Transport, socket: &Path) -> Result<Channel> {
    // Checked before dialling, because the two failures need different words:
    // "there is no daemon" is answered by starting one, "the daemon refused"
    // is answered by looking at its log. `connect_uds` alone would report both
    // as a transport error.
    if !socket.exists() {
        return Err(Classified::new(
            ExitCode::FailedPrecondition,
            format!(
                "rmaild is not running: there is no socket at {}. Start it with `mail daemon \
                 start`, or point $RMAIL_SOCKET/--socket at a running daemon.",
                socket.display()
            ),
        ));
    }
    rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to {}", transport.describe(socket)))
}

/// `--addr host:port`, with or without TLS.
async fn connect_tcp(transport: &Transport, addr: &str) -> Result<Channel> {
    let scheme = if transport.insecure { "http" } else { "https" };
    let uri = format!("{scheme}://{addr}");
    let mut endpoint = tonic::transport::Endpoint::try_from(uri.clone())
        .with_context(|| format!("--addr {addr} is not a usable endpoint ({uri})"))?
        .connect_timeout(rmail_core::transport::CONNECT_TIMEOUT);

    if !transport.insecure {
        // The same provider the rest of the workspace installs, chosen
        // explicitly rather than inferred from crate features — see
        // `rmail_core::install_crypto_provider`.
        rmail_core::transport::install_crypto_provider();
        let mut tls = ClientTlsConfig::new().with_enabled_roots();
        // An unreadable certificate is a bad *argument*, not a missing
        // resource: letting the raw `io::Error` through would classify
        // `--tls-ca /typo.pem` as NOT_FOUND(6), which reads as "the mailbox
        // does not have it" to anything branching on the code.
        if let Some(ca) = &transport.tls_ca {
            let pem = read_pem(ca, "--tls-ca")?;
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        if let (Some(cert), Some(key)) = (&transport.tls_cert, &transport.tls_key) {
            let cert_pem = read_pem(cert, "--tls-cert")?;
            let key_pem = read_pem(key, "--tls-key")?;
            tls = tls.identity(Identity::from_pem(cert_pem, key_pem));
        }
        endpoint = endpoint
            .tls_config(tls)
            .with_context(|| format!("configuring TLS for --addr {addr}"))?;
    }

    endpoint
        .connect()
        .await
        .with_context(|| format!("connecting to rmaild at {addr}"))
}

/// Read a PEM file named by a `--tls-*` flag, classified as a bad argument.
fn read_pem(path: &Path, flag: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        Classified::new(
            ExitCode::InvalidArgument,
            format!("reading {flag} {}: {e}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests;
