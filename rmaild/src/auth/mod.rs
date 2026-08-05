//! The auth `tower` layer: per-method scope enforcement for every gRPC
//! service the daemon exposes (task 38).
//!
//! This is a single cross-cutting layer applied once, at
//! `Server::builder().layer(...)`, rather than an interceptor wired into each
//! service individually — the whole point being that a service added to
//! `rmaild::lib` tomorrow is covered automatically as long as its methods
//! have rows in [`methods`], with no per-handler auth code and nothing new to
//! wire into the server builder.
//!
//! # Why not `tonic::service::Interceptor`
//!
//! Tonic's own [`tonic::service::interceptor`] is the more obvious tool for
//! "wrap every RPC in one auth check," but its closure is synchronous
//! (`FnMut(Request<()>) -> Result<Request<()>, Status>`), and verifying a
//! bearer token means an argon2id hash comparison plus a database read —
//! `Database::read`/`Database::write` are `async fn` precisely so that work
//! runs on the blocking-task pool instead of a Tokio worker thread. Calling
//! either from a sync interceptor would mean blocking a worker thread for the
//! duration of an (intentionally expensive) argon2 hash on every
//! token-authenticated request — exactly what "never block the runtime"
//! rules out. This is a hand-written [`tower::Layer`]/[`tower::Service`] pair
//! instead, whose `call` returns a real `Future` that awaits the verification
//! without blocking anything.
//!
//! # Extracting before awaiting
//!
//! `tonic::body::BoxBody` is `Send` but deliberately not `Sync` (it erases to
//! `dyn Body<..> + Send`), so a *reference* into the incoming
//! `http::Request<BoxBody>` cannot be held across an `.await` point in a
//! future this layer promises is `Send` — only owned data can. [`AuthService::call`]
//! therefore pulls the method path, bearer token, and Unix-peer trust decision
//! out of the request *synchronously*, before the async block starts, and
//! only ever awaits over those owned values. The request itself moves into
//! the async block too, but untouched until the very end, where it is either
//! handed to the inner service or dropped with the request never read again.
//!
//! # Two principals
//!
//! - **Unix-socket peer, uid matches the daemon's own** ([`admin_uid`]):
//!   implicit [`Scope::Admin`]. The socket is already `0600` (owner-only), so
//!   this is defense-in-depth, not the only gate — but it is a *kernel*-level
//!   check ([`tokio::net::unix::UCred`], populated from `getpeereid(2)` on
//!   Darwin — see `tokio`'s `net::unix::ucred::impl_macos` — or `SO_PEERCRED`
//!   on Linux) rather than trusting whatever the filesystem permission bit
//!   happened to allow at connect time.
//! - **Bearer token** (the only path when there is no Unix-peer trust, i.e.
//!   what a TCP connection would present once task 38's config-only TCP
//!   listener is actually stood up): verified via [`rmail_core::auth::verify`].
//!
//! [`admin_uid`]: AuthLayer::new

mod methods;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use rmail_core::auth::Scope;
use rmail_core::{Database, Error as RmailError};
use tonic::body::BoxBody;
use tonic::transport::server::UdsConnectInfo;
use tonic::Status;
use tower::{Layer, Service};

pub use methods::Requirement;

/// Installs [`AuthService`] around every RPC the server routes.
#[derive(Clone)]
pub struct AuthLayer {
    db: Database,
    /// The uid a Unix-socket peer must match to receive implicit admin —
    /// the daemon's own effective uid, read from the socket file it just
    /// created (see `rmaild::serve_uds_with_engine`).
    admin_uid: u32,
}

impl AuthLayer {
    /// Create a new layer. `admin_uid` is the uid a connecting Unix-socket
    /// peer must present to be trusted as admin.
    #[must_use]
    pub fn new(db: Database, admin_uid: u32) -> Self {
        Self { db, admin_uid }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            db: self.db.clone(),
            admin_uid: self.admin_uid,
        }
    }
}

/// The service [`AuthLayer`] installs.
#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    db: Database,
    admin_uid: u32,
}

impl<S> Service<http::Request<BoxBody>> for AuthService<S>
where
    S: Service<http::Request<BoxBody>, Response = http::Response<BoxBody>> + Clone + Send + 'static,
    S::Error: Send,
    S::Future: Send + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        // Clone-and-call rather than swap-then-restore: `Routes` (what this
        // layer ultimately wraps) is a stateless dispatch table, so a fresh
        // clone is exactly as ready as `self.inner` — the same pattern tonic
        // uses internally for its own per-connection service construction.
        let mut inner = self.inner.clone();
        let db = self.db.clone();
        let admin_uid = self.admin_uid;

        // Pulled out *synchronously*, before the async block: see the module
        // docs' "Extracting before awaiting" section for why a borrow of
        // `req` cannot survive the `.await` below.
        let method = req.uri().path().to_owned();
        let peer_admin = is_trusted_peer(&req, admin_uid);
        let bearer = bearer_token(&req).map(str::to_owned);

        Box::pin(async move {
            match authorize(&db, &method, peer_admin, bearer.as_deref()).await {
                Ok(()) => inner.call(req).await,
                Err(status) => Ok(status.into_http()),
            }
        })
    }
}

/// Decide whether a call to `method` may proceed, given the caller's
/// principal: implicit admin (`peer_admin`) or a `bearer` token.
async fn authorize(
    db: &Database,
    method: &str,
    peer_admin: bool,
    bearer: Option<&str>,
) -> Result<(), Status> {
    let required = match methods::lookup(method) {
        Some(Requirement::Public) => return Ok(()),
        Some(Requirement::Scope(scope)) => scope,
        None => {
            tracing::warn!(
                method,
                "denying call to a gRPC method with no scope-table entry (fail-closed default)"
            );
            return Err(Status::from(RmailError::permission_denied(format!(
                "method {method} is not registered in the capability-scope table"
            ))));
        }
    };

    let granted = principal_scopes(db, peer_admin, bearer).await?;
    if rmail_core::auth::satisfies(&granted, required) {
        Ok(())
    } else {
        Err(Status::from(RmailError::permission_denied(format!(
            "method {method} requires scope {required}, which this token does not grant"
        ))))
    }
}

/// The scopes granted to the caller: implicit admin when `peer_admin`,
/// otherwise whatever `bearer` (if present and valid) carries.
///
/// # Errors
///
/// [`Status`] (mapped from [`rmail_core::Error::Unauthenticated`]) if there is
/// neither Unix-peer trust nor a valid bearer token.
async fn principal_scopes(
    db: &Database,
    peer_admin: bool,
    bearer: Option<&str>,
) -> Result<Vec<Scope>, Status> {
    if peer_admin {
        return Ok(vec![Scope::Admin]);
    }

    let token = bearer.ok_or_else(|| {
        Status::from(RmailError::unauthenticated(
            "no Unix-peer trust and no bearer token presented",
        ))
    })?;
    let api_token = rmail_core::auth::verify(db, token)
        .await
        .map_err(Status::from)?;
    Ok(api_token.scopes)
}

/// Whether `req` arrived over a Unix-socket connection whose kernel-reported
/// peer uid matches `admin_uid`.
fn is_trusted_peer(req: &http::Request<BoxBody>, admin_uid: u32) -> bool {
    req.extensions()
        .get::<UdsConnectInfo>()
        .and_then(|info| info.peer_cred.as_ref())
        .is_some_and(|cred| cred.uid() == admin_uid)
}

/// Extract the token from an `authorization: Bearer <token>` header, if any.
fn bearer_token(req: &http::Request<BoxBody>) -> Option<&str> {
    req.headers()
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests;
