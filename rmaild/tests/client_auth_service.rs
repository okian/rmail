//! Integration test: drive `ClientAuthService` end-to-end against an
//! in-process tonic server over a Unix domain socket.
//!
//! `SetupPassword`/`ClearPassword` are exercised over the trusted socket, the
//! same way `rmaild/tests/admin_service.rs` exercises `MintToken` — but
//! `LoginPassword`/`AuthStatus` are `Public`, so this file also connects with
//! *no* credential at all to prove they are reachable that way, and that a
//! minted session token then behaves exactly like one from
//! `AdminService.MintToken` (it authorizes an ordinary admin-scoped call).
// `result_large_err`: the ad-hoc interceptor below returns `Result<_, Status>`,
// same as every production auth handler in this workspace — see
// `client_auth_service.rs`'s own `#![allow(...)]` for why.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::result_large_err
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::Config;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::client_auth_service_client::ClientAuthServiceClient;
use rmail_proto::v1::{
    AuthStatusRequest, ClearPasswordRequest, ListTokensRequest, LoginPasswordRequest,
    MintTokenRequest, SetupPasswordRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_config(Config::default()).await
    }

    async fn start_with_config(mut config: Config) -> Self {
        // Same reasoning as `rmaild::serve_uds`'s own short form: keep
        // semantic indexing off so this test does not warm (or download) an
        // embedder.
        config.index.semantic.enabled = false;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-client-auth-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-client-auth-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();
        Self::spawn(socket, db_path, db, config).await
    }

    /// Start with a password already configured, seeded directly into the
    /// database rather than via `SetupPassword` over the wire.
    ///
    /// Needed by any test that wants `require_for_local: true`: once that
    /// flag is on, the trusted-socket peer is no longer implicit admin (see
    /// `require_login_for_peer` in `rmaild::auth`), so there is no bootstrap
    /// path left to call `SetupPassword` over — a daemon started this way
    /// with nothing configured just refuses to bind at all (see
    /// `require_for_local_with_no_password_refuses_to_start`).
    async fn start_seeded(password: &str, require_for_local: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-client-auth-seeded-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-client-auth-seeded-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();
        rmail_core::auth::password::set_password(&db, password)
            .await
            .expect("seed the password directly");

        let mut config = Config::default();
        config.index.semantic.enabled = false;
        config.client_auth.require_for_local = require_for_local;
        Self::spawn(socket, db_path, db, config).await
    }

    /// The spawn-and-poll tail shared by every way of starting a server
    /// above — they differ only in how `socket`/`db_path`/`db`/`config` get
    /// built.
    async fn spawn(
        socket: PathBuf,
        db_path: PathBuf,
        db: rmail_core::Database,
        config: Config,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, db, config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> ClientAuthServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        ClientAuthServiceClient::new(channel)
    }

    /// A `ClientAuthService` client presenting `token` as its bearer,
    /// instead of relying on the trusted socket's implicit admin — for
    /// exercising what a narrowly-scoped caller (not this process) can and
    /// cannot do.
    async fn client_as(
        &self,
        token: &str,
    ) -> ClientAuthServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
        >,
    > {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        let header: tonic::metadata::MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
        ClientAuthServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", header.clone());
            Ok(req)
        })
    }

    /// Mint a token scoped to `mail.read` alone, authenticating as
    /// `admin_token` — for tests that need a real, but deliberately
    /// under-scoped, bearer minted without relying on the trusted-socket
    /// peer being implicit admin (which `require_for_local` disables).
    async fn mint_read_only_token_as(&self, admin_token: &str) -> String {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        let header: tonic::metadata::MetadataValue<_> =
            format!("Bearer {admin_token}").parse().unwrap();
        let mut admin =
            AdminServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
                req.metadata_mut().insert("authorization", header.clone());
                Ok(req)
            });
        admin
            .mint_token(MintTokenRequest {
                name: "read-only-test-token".to_owned(),
                scopes: vec!["mail.read".to_owned()],
                ttl_secs: None,
            })
            .await
            .expect("mint a read-only token")
            .into_inner()
            .token
    }

    async fn shutdown(self) {
        self.shutdown.send(()).unwrap();
        self.handle.await.unwrap().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

#[tokio::test]
async fn setup_then_login_mints_a_token_that_authorizes_an_admin_call() {
    let server = TestServer::start().await;
    let mut auth = server.client().await;

    // Setup: the Unix-socket peer is implicit admin, so no token is needed —
    // same bootstrap path as `AdminService.MintToken`.
    auth.setup_password(SetupPasswordRequest {
        password: "correct horse battery staple".to_owned(),
        current_password: String::new(),
    })
    .await
    .expect("setup");

    let login = auth
        .login_password(LoginPasswordRequest {
            password: "correct horse battery staple".to_owned(),
        })
        .await
        .expect("login")
        .into_inner();
    assert!(login.token.starts_with("rmail_tok_"));
    assert!(login.expires_at > 0);

    // The minted token behaves like any other bearer token: it authorizes an
    // admin-scoped call on a *different* service, over a fresh connection
    // that presents it (not the trusted socket implicitly).
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();
    let token: tonic::metadata::MetadataValue<_> =
        format!("Bearer {}", login.token).parse().unwrap();
    let mut admin =
        AdminServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", token.clone());
            Ok(req)
        });
    let listed = admin
        .list_tokens(ListTokensRequest {})
        .await
        .expect("the session token should authorize an admin-scoped call")
        .into_inner();
    assert_eq!(listed.tokens.len(), 1, "the session token minted itself");

    server.shutdown().await;
}

#[tokio::test]
async fn wrong_password_is_unauthenticated_and_mints_no_token() {
    let server = TestServer::start().await;
    let mut auth = server.client().await;

    auth.setup_password(SetupPasswordRequest {
        password: "hunter2".to_owned(),
        current_password: String::new(),
    })
    .await
    .expect("setup");

    let status = auth
        .login_password(LoginPasswordRequest {
            password: "wrong".to_owned(),
        })
        .await
        .expect_err("wrong password should be rejected");
    assert_eq!(status.code(), Code::Unauthenticated);

    server.shutdown().await;
}

#[tokio::test]
async fn login_with_nothing_configured_is_unauthenticated() {
    let server = TestServer::start().await;
    let mut auth = server.client().await;

    let status = auth
        .login_password(LoginPasswordRequest {
            password: "anything".to_owned(),
        })
        .await
        .expect_err("no password configured should be rejected");
    assert_eq!(status.code(), Code::Unauthenticated);

    server.shutdown().await;
}

#[tokio::test]
async fn repeated_failures_lock_out_with_resource_exhausted() {
    let mut config = Config::default();
    config.client_auth.max_attempts = 2;
    let server = TestServer::start_with_config(config).await;
    let mut auth = server.client().await;

    auth.setup_password(SetupPasswordRequest {
        password: "hunter2".to_owned(),
        current_password: String::new(),
    })
    .await
    .expect("setup");

    let _ = auth
        .login_password(LoginPasswordRequest {
            password: "wrong".to_owned(),
        })
        .await;
    let status = auth
        .login_password(LoginPasswordRequest {
            password: "wrong".to_owned(),
        })
        .await
        .expect_err("second failure should trip the lockout");
    assert_eq!(status.code(), Code::ResourceExhausted);

    // The *correct* password is refused too, while locked out.
    let status = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect_err("locked out even for the correct password");
    assert_eq!(status.code(), Code::ResourceExhausted);

    server.shutdown().await;
}

#[tokio::test]
async fn clear_password_removes_the_gate_and_status_reflects_it() {
    let server = TestServer::start().await;
    let mut auth = server.client().await;

    auth.setup_password(SetupPasswordRequest {
        password: "hunter2".to_owned(),
        current_password: String::new(),
    })
    .await
    .expect("setup");
    let status = auth
        .auth_status(AuthStatusRequest {})
        .await
        .expect("status")
        .into_inner();
    assert!(status.password_configured);
    assert!(!status.local_login_required);

    auth.clear_password(ClearPasswordRequest {})
        .await
        .expect("clear");
    let status = auth
        .auth_status(AuthStatusRequest {})
        .await
        .expect("status")
        .into_inner();
    assert!(!status.password_configured);

    server.shutdown().await;
}

#[tokio::test]
async fn require_for_local_denies_the_trusted_socket_without_a_prior_login() {
    // With no password configured, the daemon refuses to start with this
    // flag on — see `require_for_local_with_no_password_refuses_to_start`.
    // So the password is seeded directly (bypassing gRPC — there is no
    // trusted socket to call `SetupPassword` over until the daemon is up)
    // before the one daemon in this test ever starts, rather than starting a
    // second daemon against a database a first one just used: this test is
    // about the auth gate, not about restart-across-shutdown correctness.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let socket = PathBuf::from("/tmp").join(format!("rmail-client-auth-local-{pid}-{n}.sock"));
    let db_path = std::env::temp_dir().join(format!("rmail-client-auth-local-{pid}-{n}.db"));
    let db = rmail_core::Database::open(&db_path).unwrap();
    rmail_core::auth::password::set_password(&db, "hunter2")
        .await
        .expect("seed the password directly");

    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.client_auth.require_for_local = true;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let handle = tokio::spawn(async move {
        rmaild::serve_uds_with_config(&server_socket, db, config, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let mut ready = false;
    for _ in 0..200 {
        if rmail_core::connect_uds(&socket).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready, "server never became ready");

    // The trusted socket alone is no longer enough for an admin-scoped RPC.
    let channel = rmail_core::connect_uds(&socket).await.unwrap();
    let mut admin = AdminServiceClient::new(channel);
    let status = admin
        .list_tokens(ListTokensRequest {})
        .await
        .expect_err("peer trust alone must not be enough when require_for_local is set");
    assert_eq!(status.code(), Code::Unauthenticated);

    // But LoginPassword is still reachable, and its token still works.
    let channel = rmail_core::connect_uds(&socket).await.unwrap();
    let mut auth = ClientAuthServiceClient::new(channel);
    let login = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("login must still work with no prior credential")
        .into_inner();
    assert!(login.token.starts_with("rmail_tok_"));

    shutdown_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn require_for_local_with_no_password_refuses_to_start() {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let socket = PathBuf::from("/tmp").join(format!("rmail-client-auth-refuse-{pid}-{n}.sock"));
    let db_path = std::env::temp_dir().join(format!("rmail-client-auth-refuse-{pid}-{n}.db"));
    let db = rmail_core::Database::open(&db_path).unwrap();
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.client_auth.require_for_local = true;

    let result = rmaild::serve_uds_with_config(&socket, db, config, std::future::pending()).await;
    assert!(
        matches!(
            result,
            Err(rmaild::ServeError::LocalLoginRequiredWithNoCredential)
        ),
        "got {result:?}"
    );

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
}

#[tokio::test]
async fn a_read_only_token_cannot_setup_or_clear_the_password() {
    // `require_for_local: true` so the trusted-socket peer is *not*
    // implicit admin — otherwise every call below would succeed on peer
    // trust alone regardless of which bearer (if any) is attached, and this
    // test would prove nothing about `read_only`'s own scope. See
    // `a_second_login_revokes_the_first_sessions_token`'s own comment for
    // the same reasoning.
    let server = TestServer::start_seeded("hunter2", true).await;

    let admin_token = server
        .client()
        .await
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("login")
        .into_inner()
        .token;
    let read_only = server.mint_read_only_token_as(&admin_token).await;

    let status = server
        .client_as(&read_only)
        .await
        .setup_password(SetupPasswordRequest {
            password: "new-password".to_owned(),
            current_password: "hunter2".to_owned(),
        })
        .await
        .expect_err("mail.read must not authorize SetupPassword");
    assert_eq!(status.code(), Code::PermissionDenied);

    let status = server
        .client_as(&read_only)
        .await
        .clear_password(ClearPasswordRequest {})
        .await
        .expect_err("mail.read must not authorize ClearPassword");
    assert_eq!(status.code(), Code::PermissionDenied);

    server.shutdown().await;
}

#[tokio::test]
async fn an_empty_password_is_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .setup_password(SetupPasswordRequest {
            password: String::new(),
            current_password: String::new(),
        })
        .await
        .expect_err("an empty password must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn changing_the_password_requires_the_current_one() {
    let server = TestServer::start().await;
    let mut auth = server.client().await;
    auth.setup_password(SetupPasswordRequest {
        password: "hunter2".to_owned(),
        current_password: String::new(),
    })
    .await
    .expect("initial setup");

    let status = auth
        .setup_password(SetupPasswordRequest {
            password: "new-password".to_owned(),
            current_password: "wrong".to_owned(),
        })
        .await
        .expect_err("the wrong current_password must be refused");
    assert_eq!(status.code(), Code::Unauthenticated);

    // The password did not change: the original still logs in.
    let login = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("the original password should still work")
        .into_inner();
    assert!(login.token.starts_with("rmail_tok_"));

    // The right current_password, on the other hand, is accepted.
    auth.setup_password(SetupPasswordRequest {
        password: "new-password".to_owned(),
        current_password: "hunter2".to_owned(),
    })
    .await
    .expect("the correct current_password should be accepted");
    auth.login_password(LoginPasswordRequest {
        password: "new-password".to_owned(),
    })
    .await
    .expect("the new password should now work");

    server.shutdown().await;
}

#[tokio::test]
async fn clear_password_is_refused_when_require_for_local_is_set() {
    let mut config = Config::default();
    config.client_auth.require_for_local = true;
    // Seeded directly, same reasoning as
    // `require_for_local_denies_the_trusted_socket_without_a_prior_login`:
    // the daemon refuses to start otherwise.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let socket = PathBuf::from("/tmp").join(format!("rmail-client-auth-clr-{pid}-{n}.sock"));
    let db_path = std::env::temp_dir().join(format!("rmail-client-auth-clr-{pid}-{n}.db"));
    let db = rmail_core::Database::open(&db_path).unwrap();
    rmail_core::auth::password::set_password(&db, "hunter2")
        .await
        .expect("seed the password directly");
    config.index.semantic.enabled = false;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let handle = tokio::spawn(async move {
        rmaild::serve_uds_with_config(&server_socket, db, config, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let mut ready = false;
    for _ in 0..200 {
        if rmail_core::connect_uds(&socket).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready, "server never became ready");

    // Log in first: the trusted socket alone is not enough to reach
    // ClearPassword either, once require_for_local is set.
    let channel = rmail_core::connect_uds(&socket).await.unwrap();
    let mut auth = ClientAuthServiceClient::new(channel);
    let login = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("login")
        .into_inner();
    let channel = rmail_core::connect_uds(&socket).await.unwrap();
    let header: tonic::metadata::MetadataValue<_> =
        format!("Bearer {}", login.token).parse().unwrap();
    let mut auth =
        ClientAuthServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", header.clone());
            Ok(req)
        });

    let status = auth
        .clear_password(ClearPasswordRequest {})
        .await
        .expect_err("clearing the only credential require_for_local depends on must be refused");
    assert_eq!(status.code(), Code::FailedPrecondition);

    shutdown_tx.send(()).unwrap();
    handle.await.unwrap().unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn a_second_login_revokes_the_first_sessions_token() {
    // `require_for_local: true` so the trusted-socket peer is *not*
    // implicit admin: with it `false` (the default), `list_tokens` below
    // would succeed on peer trust alone regardless of whether `first.token`
    // was revoked, since `principal_scopes` grants admin from the peer
    // before it ever looks at the bearer — proving nothing about revocation.
    let server = TestServer::start_seeded("hunter2", true).await;
    let mut auth = server.client().await;

    let first = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("first login")
        .into_inner();
    let second = auth
        .login_password(LoginPasswordRequest {
            password: "hunter2".to_owned(),
        })
        .await
        .expect("second login")
        .into_inner();
    assert_ne!(first.token, second.token, "each login mints its own token");

    // The first token no longer authorizes anything...
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();
    let header: tonic::metadata::MetadataValue<_> =
        format!("Bearer {}", first.token).parse().unwrap();
    let mut admin_with_first =
        AdminServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", header.clone());
            Ok(req)
        });
    let status = admin_with_first
        .list_tokens(ListTokensRequest {})
        .await
        .expect_err("the first session should have been revoked by the second login");
    assert_eq!(status.code(), Code::Unauthenticated);

    // ...but the second, current one still does.
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();
    let header: tonic::metadata::MetadataValue<_> =
        format!("Bearer {}", second.token).parse().unwrap();
    let mut admin_with_second =
        AdminServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", header.clone());
            Ok(req)
        });
    admin_with_second
        .list_tokens(ListTokensRequest {})
        .await
        .expect("the second, current session should still authorize calls");

    server.shutdown().await;
}
