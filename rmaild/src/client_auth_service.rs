//! The `ClientAuthService` gRPC implementation.
//!
//! `SetupPassword`/`ClearPassword` wrap [`rmail_core::auth::password`]
//! directly, the same way `AdminApi` wraps [`rmail_core::auth`]'s token
//! lifecycle — no scope check in this file for either, since the daemon-wide
//! auth layer already enforces `admin` for both (see
//! `rmaild::auth::methods`).
//!
//! `LoginPassword` is the one handler here that *is* the authorization
//! check: [`rmail_core::auth::password::verify_password`] is Argon2id in
//! constant time with a persisted lockout, and this function's only job is
//! to translate its outcome into the right `tonic::Status` and, on success,
//! mint an ordinary capability token via [`rmail_core::auth::mint`] — the
//! same call `AdminApi::mint_token` makes. Everything downstream of that
//! mint (verification, scope enforcement, revocation) is unaware a password
//! was ever involved.
#![allow(clippy::result_large_err)]

use rmail_core::auth::password::{self, LoginOutcome};
use rmail_core::auth::{list as list_tokens, mint, revoke, NewToken, Scope};
use rmail_core::config::ClientAuthConfig;
use rmail_core::{Database, Error};
use rmail_proto::v1::client_auth_service_server::ClientAuthService;
use rmail_proto::v1::{
    AuthStatusRequest, AuthStatusResponse, ClearPasswordRequest, ClearPasswordResponse,
    LoginPasswordRequest, LoginPasswordResponse, SetupPasswordRequest, SetupPasswordResponse,
};
use tonic::{Request, Response, Status};

/// The label every `LoginPassword`-minted token carries.
///
/// Fixed rather than derived from the request (there is nothing in a
/// [`LoginPasswordRequest`] to derive one from) so `mail token list` — which
/// reads the same `api_tokens` table `AdminService.MintToken` writes — shows
/// at a glance which rows came from a password login rather than from an
/// operator running `mail token create` by hand.
const SESSION_TOKEN_NAME: &str = "client-auth-session";

/// The `ClientAuthService` handler.
#[derive(Clone)]
pub struct ClientAuthApi {
    db: Database,
    config: ClientAuthConfig,
}

impl ClientAuthApi {
    /// Create a handler over the given database and `[client_auth]` config.
    #[must_use]
    pub fn new(db: Database, config: ClientAuthConfig) -> Self {
        Self { db, config }
    }

    /// `client_auth.lockout`, in whole seconds, saturating rather than
    /// panicking on a config value absurd enough to overflow `i64`.
    fn lockout_secs(&self) -> i64 {
        i64::try_from(self.config.lockout.as_duration().as_secs()).unwrap_or(i64::MAX)
    }

    /// `client_auth.session_ttl`, in whole seconds, same saturation as
    /// [`Self::lockout_secs`].
    fn session_ttl_secs(&self) -> i64 {
        i64::try_from(self.config.session_ttl.as_duration().as_secs()).unwrap_or(i64::MAX)
    }
}

#[tonic::async_trait]
impl ClientAuthService for ClientAuthApi {
    async fn setup_password(
        &self,
        request: Request<SetupPasswordRequest>,
    ) -> Result<Response<SetupPasswordResponse>, Status> {
        let req = request.into_inner();
        // A session token minted by LoginPassword is meant to be a
        // temporary, revocable credential — requiring the *current*
        // password to set a new one is what stops a leaked session token
        // from turning itself into a permanent takeover. Not required on
        // first-time setup: `SetupPasswordRequest.current_password`'s own
        // doc explains why there is nothing to confirm yet.
        if password::is_configured(&self.db).await? {
            let confirmed = password::verify_current(&self.db, &req.current_password).await?;
            if !confirmed {
                return Err(Status::from(Error::unauthenticated(
                    "current_password does not match the configured password",
                )));
            }
        }
        let updated_at = password::set_password(&self.db, &req.password).await?;
        tracing::info!("client-auth password set");
        Ok(Response::new(SetupPasswordResponse { updated_at }))
    }

    async fn clear_password(
        &self,
        _request: Request<ClearPasswordRequest>,
    ) -> Result<Response<ClearPasswordResponse>, Status> {
        // With `require_for_local` on, clearing the only credential that can
        // ever satisfy it is indistinguishable from bricking the daemon:
        // `LoginPassword` would answer every future call with `NotConfigured`,
        // peer trust is off, and the *next restart* refuses to bind at all
        // (`ServeError::LocalLoginRequiredWithNoCredential`) — with no local
        // way back short of editing the database by hand. Refused here,
        // before that state is ever reached, for the same reason the startup
        // check exists at all: `SetupPassword` (replace it with a new
        // password) remains available, only deleting it outright is refused.
        if self.config.require_for_local {
            return Err(Status::from(Error::failed_precondition(
                "client_auth.require_for_local is true; clearing the password would lock out \
                 every local client, including this one, on the next restart. Set \
                 require_for_local to false first, or replace the password with \
                 SetupPassword/`mail auth setup` instead of clearing it.",
            )));
        }
        password::clear_password(&self.db).await?;
        tracing::info!("client-auth password cleared");
        Ok(Response::new(ClearPasswordResponse { cleared: true }))
    }

    async fn login_password(
        &self,
        request: Request<LoginPasswordRequest>,
    ) -> Result<Response<LoginPasswordResponse>, Status> {
        let req = request.into_inner();
        let outcome = password::verify_password(
            &self.db,
            &req.password,
            self.config.max_attempts,
            self.lockout_secs(),
        )
        .await?;

        match outcome {
            LoginOutcome::Success => {
                revoke_previous_sessions(&self.db).await?;
                let minted = mint(
                    &self.db,
                    NewToken {
                        name: SESSION_TOKEN_NAME.to_owned(),
                        scopes: vec![Scope::Admin],
                        ttl_secs: Some(self.session_ttl_secs()),
                    },
                )
                .await?;
                tracing::info!(token_id = minted.token.id, "client-auth login succeeded");
                Ok(Response::new(LoginPasswordResponse {
                    token: minted.secret,
                    // `ttl_secs: Some(_)` above guarantees `mint` always sets
                    // an expiry — see `rmail_core::auth::mint`.
                    expires_at: minted.token.expires_at.unwrap_or_default(),
                    id: minted.token.id,
                }))
            }
            LoginOutcome::WrongPassword { remaining } => {
                tracing::warn!(remaining, "client-auth login: wrong password");
                Err(Status::from(Error::unauthenticated("invalid password")))
            }
            LoginOutcome::LockedOut { retry_after_secs } => {
                tracing::warn!(retry_after_secs, "client-auth login: locked out");
                Err(Status::from(Error::resource_exhausted(format!(
                    "too many failed attempts; try again in {retry_after_secs}s"
                ))))
            }
            LoginOutcome::NotConfigured => Err(Status::from(Error::unauthenticated(
                "no password is configured; an admin must run `mail auth setup` first",
            ))),
        }
    }

    async fn auth_status(
        &self,
        _request: Request<AuthStatusRequest>,
    ) -> Result<Response<AuthStatusResponse>, Status> {
        let password_configured = password::is_configured(&self.db).await?;
        Ok(Response::new(AuthStatusResponse {
            password_configured,
            local_login_required: self.config.require_for_local,
        }))
    }
}

/// Revoke every not-yet-revoked token named [`SESSION_TOKEN_NAME`] before
/// minting a fresh one. Revoking one that has already expired is a harmless
/// no-op ([`revoke`] is idempotent), so this does not bother checking first.
///
/// Without this, each `mail auth login` mints a brand new `Scope::Admin` /
/// `session_ttl`-lived token and the client's local cache
/// (`rmail-cli::session`) simply overwrites its record of the previous one —
/// which does not revoke it, only stops anything from *reminding* the
/// operator it still exists. Every prior login accumulates an orphaned admin
/// credential that stays live for the rest of its `session_ttl`, discoverable
/// only via `mail token list`. A password login is meant to represent *one*
/// active session per client, the same way logging into most things replaces
/// rather than stacks; this is what makes that true.
///
/// Matches by name, not by scope or by "was this minted by LoginPassword" —
/// there is no such marker, and name is the one field
/// [`rmail_core::auth::mint`] lets a caller choose, which is exactly why
/// [`SESSION_TOKEN_NAME`] is a fixed constant rather than derived per
/// request: two different login sessions must collide on it for this to find
/// them.
async fn revoke_previous_sessions(db: &Database) -> Result<(), Status> {
    let tokens = list_tokens(db).await?;
    for token in tokens {
        if token.name == SESSION_TOKEN_NAME && !token.revoked {
            revoke(db, token.id).await?;
            tracing::info!(
                token_id = token.id,
                "revoked the previous client-auth session on a new login"
            );
        }
    }
    Ok(())
}
