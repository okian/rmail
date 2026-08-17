//! Auth layer tests: the scope allow/deny matrix, the Unix-peer-uid path, and
//! revoked/absent-token rejection — driven against the real [`AuthService`],
//! not just the scope-satisfaction logic it calls into, so a "physically
//! denied" claim means the request never reached the wrapped service.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use rmail_core::auth::{mint, revoke, NewToken, Scope};
use rmail_core::Database;
use tonic::body::BoxBody;
use tonic::transport::server::UdsConnectInfo;
use tonic::Status;
use tower::{Layer, Service};

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-authlayer-{pid}-{n}.db"));
        let db = Database::open(&path).expect("open temp db");
        Self { db, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// An inner service that records how many times it was called and always
/// succeeds — the auth layer's job is to decide whether this is ever reached.
#[derive(Clone, Default)]
struct CountingInner {
    calls: Arc<AtomicUsize>,
}

impl Service<http::Request<BoxBody>> for CountingInner {
    type Response = http::Response<BoxBody>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: http::Request<BoxBody>) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(http::Response::new(tonic::body::empty_body())))
    }
}

fn synthetic_request(method: &str) -> http::Request<BoxBody> {
    http::Request::builder()
        .uri(method)
        .body(tonic::body::empty_body())
        .expect("build synthetic request")
}

fn with_bearer(mut req: http::Request<BoxBody>, token: &str) -> http::Request<BoxBody> {
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("valid header value"),
    );
    req
}

fn with_uds_peer(
    mut req: http::Request<BoxBody>,
    cred: tokio::net::unix::UCred,
) -> http::Request<BoxBody> {
    req.extensions_mut().insert(UdsConnectInfo {
        peer_addr: None,
        peer_cred: Some(cred),
    });
    req
}

/// A genuine [`tokio::net::unix::UCred`] for use as a synthetic peer identity:
/// `UCred` has no public constructor (by design — it should only ever come
/// from a real socket), so this binds a loopback Unix socket pair and reads
/// the credential tonic itself would populate, off the accepted side. On
/// Darwin this exercises the same `getpeereid(2)` path tokio's
/// `UnixStream::peer_cred` uses; the test asserting a *mismatched* admin uid
/// does not need a second, differently-privileged process to prove the
/// comparison actually gates something — see
/// `a_mismatched_peer_uid_does_not_get_implicit_admin`.
async fn real_peer_cred() -> tokio::net::unix::UCred {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rmail-authlayer-peer-{pid}-{n}.sock"));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).expect("bind loopback socket");
    let connect = tokio::net::UnixStream::connect(&path);
    let accept = listener.accept();
    let (connected, accepted) = tokio::join!(connect, accept);
    let _client = connected.expect("connect to loopback socket");
    let (server_side, _addr) = accepted.expect("accept loopback connection");
    let cred = server_side.peer_cred().expect("peer_cred via getpeereid");
    let _ = std::fs::remove_file(&path);
    cred
}

/// Run `req` through a fresh `AuthLayer(admin_uid) -> CountingInner`, with
/// `client_auth.require_for_local` at its default (`false`) — i.e. every
/// test in this file except the ones specifically about that flag, which use
/// [`run_with_local_gate`] instead.
async fn run(
    db: &Database,
    admin_uid: u32,
    req: http::Request<BoxBody>,
) -> (http::Response<BoxBody>, Arc<AtomicUsize>) {
    run_with_local_gate(db, admin_uid, false, req).await
}

/// [`run`], with `require_login_for_peer` given explicitly.
async fn run_with_local_gate(
    db: &Database,
    admin_uid: u32,
    require_login_for_peer: bool,
    req: http::Request<BoxBody>,
) -> (http::Response<BoxBody>, Arc<AtomicUsize>) {
    let counting = CountingInner::default();
    let calls = Arc::clone(&counting.calls);
    let layer = AuthLayer::new(db.clone(), admin_uid, require_login_for_peer);
    let mut svc = layer.layer(counting);
    let response = svc
        .call(req)
        .await
        .expect("AuthService::call is infallible");
    (response, calls)
}

/// Whether `response` is a gRPC error (i.e. the auth layer short-circuited
/// rather than forwarding to the inner service).
fn status_of(response: &http::Response<BoxBody>) -> Option<Status> {
    Status::from_header_map(response.headers())
}

#[tokio::test]
async fn a_public_method_bypasses_auth_and_reaches_inner() {
    let tmp = TempDb::open();
    let req = synthetic_request("/grpc.health.v1.Health/Check");
    let (response, calls) = run(&tmp.db, 0, req).await;

    assert!(status_of(&response).is_none(), "should not be an error");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unregistered_method_is_denied_and_never_reaches_inner() {
    let tmp = TempDb::open();
    let req = synthetic_request("/rmail.v1.DoesNotExist/Method");
    let (response, calls) = run(&tmp.db, 0, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "inner must not be called");
}

#[tokio::test]
async fn no_trust_and_no_token_is_unauthenticated() {
    let tmp = TempDb::open();
    let req = synthetic_request("/rmail.v1.AdminService/ListTokens");
    let (response, calls) = run(&tmp.db, 0, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_trusted_unix_peer_gets_implicit_admin() {
    let tmp = TempDb::open();
    let cred = real_peer_cred().await;
    // `UCred` is `Copy`, so `cred` is still usable below for `.uid()`.
    let req = with_uds_peer(synthetic_request("/rmail.v1.AdminService/MintToken"), cred);
    let (response, calls) = run(&tmp.db, cred.uid(), req).await;

    assert!(
        status_of(&response).is_none(),
        "a matching peer uid should be granted admin"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_mismatched_peer_uid_does_not_get_implicit_admin() {
    let tmp = TempDb::open();
    let cred = real_peer_cred().await;
    let req = with_uds_peer(synthetic_request("/rmail.v1.AdminService/MintToken"), cred);
    // The layer is configured to trust a *different* uid than the peer's —
    // the 0600 socket permission is not the only gate.
    let (response, calls) = run(&tmp.db, cred.uid().wrapping_add(1), req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn require_login_for_peer_denies_a_matching_uid_with_no_token() {
    let tmp = TempDb::open();
    let cred = real_peer_cred().await;
    let req = with_uds_peer(synthetic_request("/rmail.v1.AdminService/MintToken"), cred);
    // Same matching peer uid as `a_trusted_unix_peer_gets_implicit_admin`, but
    // with the local shortcut turned off: kernel-level trust is no longer
    // enough on its own.
    let (response, calls) = run_with_local_gate(&tmp.db, cred.uid(), true, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn require_login_for_peer_still_accepts_a_valid_bearer_token_from_that_peer() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "local-session".to_owned(),
            scopes: vec![Scope::Admin],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint a token");
    let cred = real_peer_cred().await;
    let req = with_bearer(
        with_uds_peer(synthetic_request("/rmail.v1.AdminService/MintToken"), cred),
        &minted.secret,
    );
    let (response, calls) = run_with_local_gate(&tmp.db, cred.uid(), true, req).await;

    assert!(
        status_of(&response).is_none(),
        "a valid bearer token must still work even though the peer's own \
         kernel-level trust does not count here"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_read_only_token_is_physically_denied_delete_and_send() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "read-only".to_owned(),
            scopes: vec![Scope::MailRead],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    // The acceptance case task 39 names explicitly: a read-only token must be
    // physically denied every mutating MailService RPC, not merely the two
    // originally exercised here ahead of the real service landing.
    for method in [
        "/rmail.v1.MailService/Delete",
        "/rmail.v1.MailService/Move",
        "/rmail.v1.MailService/Copy",
        "/rmail.v1.MailService/SetFlags",
        "/rmail.v1.OutboxService/Send",
    ] {
        let req = with_bearer(synthetic_request(method), &minted.secret);
        let (response, calls) = run(&tmp.db, 0, req).await;

        let status =
            status_of(&response).unwrap_or_else(|| unreachable!("{method} should be denied"));
        assert_eq!(
            status.code(),
            tonic::Code::PermissionDenied,
            "method {method}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{method}: inner must not be called — physically denied"
        );
    }
}

#[tokio::test]
async fn a_token_with_the_right_scope_reaches_inner() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "writer".to_owned(),
            scopes: vec![Scope::MailWrite],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    let req = with_bearer(
        synthetic_request("/rmail.v1.MailService/Delete"),
        &minted.secret,
    );
    let (response, calls) = run(&tmp.db, 0, req).await;

    assert!(status_of(&response).is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// `Requirement::AllOf` is a conjunction, driven through the real layer.
///
/// Each half of `EvaluateRules`' requirement on its own is a token that could
/// otherwise archive an inbox (`automation` alone) or spawn a hook process
/// (`mail.write` alone). The point of the row is that neither does.
#[tokio::test]
async fn neither_half_of_an_all_of_requirement_is_enough_on_its_own() {
    let tmp = TempDb::open();
    for (name, scopes) in [
        ("automation-only", vec![Scope::Automation]),
        ("write-only", vec![Scope::MailWrite]),
        (
            "read-and-automation",
            vec![Scope::MailRead, Scope::Automation],
        ),
    ] {
        let minted = mint(
            &tmp.db,
            NewToken {
                name: name.to_owned(),
                scopes,
                ttl_secs: None,
            },
        )
        .await
        .expect("mint");

        for method in [
            "/rmail.v1.RuleService/EvaluateRules",
            "/rmail.v1.RuleService/CreateRule",
        ] {
            let req = with_bearer(synthetic_request(method), &minted.secret);
            let (response, calls) = run(&tmp.db, 0, req).await;
            let status = status_of(&response)
                .unwrap_or_else(|| unreachable!("{name} should be denied {method}"));
            assert_eq!(
                status.code(),
                tonic::Code::PermissionDenied,
                "{name} on {method}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{name} on {method}: inner must not be called — physically denied"
            );
        }
    }
}

#[tokio::test]
async fn holding_every_scope_of_an_all_of_requirement_reaches_inner() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "automation-writer".to_owned(),
            scopes: vec![Scope::Automation, Scope::MailWrite, Scope::AiInvoke],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    let req = with_bearer(
        synthetic_request("/rmail.v1.RuleService/EvaluateRules"),
        &minted.secret,
    );
    let (response, calls) = run(&tmp.db, 0, req).await;
    assert!(status_of(&response).is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // ...and dropping any one of the three takes it away again. Asserted per
    // scope so a future re-scoping that quietly drops one from the row does
    // not leave this test still passing on the other two.
    for missing in [Scope::Automation, Scope::MailWrite, Scope::AiInvoke] {
        let scopes: Vec<Scope> = [Scope::Automation, Scope::MailWrite, Scope::AiInvoke]
            .into_iter()
            .filter(|s| *s != missing)
            .collect();
        let partial = mint(
            &tmp.db,
            NewToken {
                name: format!("without-{missing}"),
                scopes,
                ttl_secs: None,
            },
        )
        .await
        .expect("mint");
        let req = with_bearer(
            synthetic_request("/rmail.v1.RuleService/EvaluateRules"),
            &partial.secret,
        );
        let (response, calls) = run(&tmp.db, 0, req).await;
        assert_eq!(
            status_of(&response).map(|s| s.code()),
            Some(tonic::Code::PermissionDenied),
            "a token without {missing} must not fire rules"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

/// Listing rules is deliberately the low-water mark of this service: an
/// `automation`-only token reads the rule list and can do nothing else here.
#[tokio::test]
async fn an_automation_token_may_list_rules_but_not_fire_one() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "read-automation".to_owned(),
            scopes: vec![Scope::Automation],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    let req = with_bearer(
        synthetic_request("/rmail.v1.RuleService/ListRules"),
        &minted.secret,
    );
    let (response, calls) = run(&tmp.db, 0, req).await;
    assert!(status_of(&response).is_none(), "listing must be allowed");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_mail_scoped_token_cannot_reach_admin_methods() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "not-admin".to_owned(),
            scopes: vec![Scope::MailRead, Scope::MailWrite, Scope::MailSend],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    let req = with_bearer(
        synthetic_request("/rmail.v1.AdminService/MintToken"),
        &minted.secret,
    );
    let (response, calls) = run(&tmp.db, 0, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_revoked_token_is_rejected() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "will-be-revoked".to_owned(),
            scopes: vec![Scope::Admin],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint");

    // Valid before revocation.
    let req = with_bearer(
        synthetic_request("/rmail.v1.AdminService/ListTokens"),
        &minted.secret,
    );
    let (response, _) = run(&tmp.db, 0, req).await;
    assert!(
        status_of(&response).is_none(),
        "should be valid before revoke"
    );

    revoke(&tmp.db, minted.token.id).await.expect("revoke");

    let req = with_bearer(
        synthetic_request("/rmail.v1.AdminService/ListTokens"),
        &minted.secret,
    );
    let (response, calls) = run(&tmp.db, 0, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "revoked token must not reach inner"
    );
}

#[tokio::test]
async fn an_unknown_bearer_token_is_unauthenticated() {
    let tmp = TempDb::open();
    let req = with_bearer(
        synthetic_request("/rmail.v1.AdminService/ListTokens"),
        &format!("rmail_tok_999999_{}", "a".repeat(64)),
    );
    let (response, calls) = run(&tmp.db, 0, req).await;

    let status = status_of(&response).expect("should be a gRPC error");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
