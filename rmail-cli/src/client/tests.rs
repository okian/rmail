//! The transport flags: what combinations are refused, and what the
//! interceptor puts on the wire.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tonic::service::Interceptor as _;

use super::*;

fn flags() -> Transport {
    Transport::default()
}

/// A client certificate is a pair; half of one is a mistake worth catching
/// before any connection is attempted.
#[test]
fn a_client_certificate_without_its_key_is_refused() {
    let error = Transport::new(
        Some("h:1".to_owned()),
        None,
        None,
        None,
        Some("cert.pem".into()),
        None,
        false,
    )
    .expect_err("half a client certificate must not be accepted");
    assert!(format!("{error:#}").contains("--tls-key"), "{error:#}");
    assert_eq!(ExitCode::of(&error), ExitCode::Usage);
}

/// TLS flags describe a TCP connection. Silently ignoring them against a Unix
/// socket would let someone believe a local connection was authenticated by a
/// certificate it never presented.
#[test]
fn tls_flags_without_an_addr_are_refused() {
    for (ca, insecure) in [(Some("ca.pem".into()), false), (None, true)] {
        let error = Transport::new(None, None, None, ca, None, None, insecure)
            .expect_err("TLS options need --addr");
        assert!(format!("{error:#}").contains("--addr"), "{error:#}");
        assert_eq!(ExitCode::of(&error), ExitCode::Usage);
    }
}

/// `--insecure` and `--tls-ca` ask for opposite things.
#[test]
fn insecure_and_tls_together_are_refused() {
    let error = Transport::new(
        Some("h:1".to_owned()),
        None,
        None,
        Some("ca.pem".into()),
        None,
        None,
        true,
    )
    .expect_err("--insecure contradicts --tls-ca");
    assert!(format!("{error:#}").contains("--insecure"), "{error:#}");
    assert_eq!(ExitCode::of(&error), ExitCode::Usage);
}

/// `--deadline 0` is an already-expired request, not "no deadline".
#[test]
fn a_zero_deadline_is_refused_rather_than_read_as_unlimited() {
    let error = Transport::new(None, None, Some(0), None, None, None, false)
        .expect_err("--deadline 0 must not be accepted");
    assert!(format!("{error:#}").contains("--deadline"), "{error:#}");
    // Usage(2), not the generic Failure(1): the fix is to retype the command
    // line, and a script that retried this identically would fail identically.
    assert_eq!(ExitCode::of(&error), ExitCode::Usage);

    let ok = Transport::new(None, None, Some(30), None, None, None, false).unwrap();
    assert_eq!(ok.deadline, Some(Duration::from_secs(30)));
}

/// The default is the Unix socket with no credential and no deadline — the
/// behaviour `mail` had before any of these flags existed.
#[test]
fn the_default_transport_is_the_unix_socket() {
    let transport = flags();
    assert!(transport.addr.is_none());
    assert!(transport.token.is_none());
    assert!(transport.deadline.is_none());
    assert!(!transport.insecure);
}

/// The token becomes an `authorization` header on every request, and the
/// deadline becomes a `grpc-timeout` — the two things that have to be true of
/// *every* RPC for the global flags to mean anything.
#[test]
fn the_interceptor_attaches_the_token_and_the_deadline() {
    let mut decorate = Decorate {
        token: Some("Bearer rmail_tok_secret".parse().unwrap()),
        deadline: Some(Duration::from_secs(7)),
    };

    let request = decorate.call(tonic::Request::new(())).unwrap();
    assert_eq!(
        request
            .metadata()
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer rmail_tok_secret")
    );
    // tonic renders the deadline as `grpc-timeout`, which is how the *server*
    // learns to stop working — a local timeout alone would leave it computing.
    assert!(
        request.metadata().get("grpc-timeout").is_some(),
        "a --deadline must travel as a gRPC deadline, not only as a local timeout"
    );
}

/// With neither flag the interceptor adds nothing, so an unauthenticated local
/// invocation looks exactly as it did before this module existed.
#[test]
fn the_interceptor_is_a_no_op_without_flags() {
    let mut decorate = Decorate::default();
    let request = decorate.call(tonic::Request::new(())).unwrap();
    assert!(request.metadata().get("authorization").is_none());
    assert!(request.metadata().get("grpc-timeout").is_none());
}

/// A token that cannot be an HTTP header is an operator mistake worth naming
/// — and the message must not echo the secret into a log or a CI transcript.
// `connect_lazy` builds a hyper connection pool, which needs a reactor even
// though nothing is dialled — so this one is an async test despite touching no
// I/O.
#[tokio::test]
async fn an_unusable_token_is_reported_without_echoing_it() {
    let parts = Parts {
        channel: tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        token: Some("secret\nX-Injected: yes".to_owned()),
        deadline: None,
    };
    let error = parts
        .into_client()
        .expect_err("a header-unsafe token must be refused");
    let rendered = format!("{error:#}");
    assert_eq!(ExitCode::of(&error), ExitCode::InvalidArgument);
    assert!(
        !rendered.contains("secret"),
        "the token must not appear in the error: {rendered}"
    );
}

/// The one guarantee `mail` makes with no daemon: a fast, specific refusal
/// rather than a hang or a generic transport error.
#[tokio::test]
async fn a_missing_socket_is_failed_precondition_and_names_the_fix() {
    let socket = std::env::temp_dir().join(format!(
        "rmail-cli-absent-{}-{}.sock",
        std::process::id(),
        line!()
    ));
    assert!(!socket.exists());

    let started = std::time::Instant::now();
    let error = connect(&socket)
        .await
        .expect_err("there is no daemon at that path");
    assert_eq!(ExitCode::of(&error), ExitCode::FailedPrecondition);
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("mail daemon start"),
        "the refusal must name the fix: {rendered}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "`mail` with no daemon must fail immediately, not hang: took {:?}",
        started.elapsed()
    );
}
