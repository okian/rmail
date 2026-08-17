//! `mail auth ...` — client_auth: password access to rmail's own API,
//! distinct from `mail account` (IMAP/SMTP credentials).
//!
//! `Setup`/`Clear` are thin wrappers over `ClientAuthService`, the same
//! shape `mail token create/list/revoke` already has over `AdminService`.
//! `Login`/`Logout` are the pair with no `AdminService` analogue: they own
//! the local session cache (`crate::session`) that lets every other command
//! skip `--token` once `mail auth login` has run — see `client.rs`'s "The
//! client_auth session cache is a fallback, not a third flag".

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::client_auth_service_client::ClientAuthServiceClient;
use rmail_proto::v1::{
    AuthStatusRequest, ClearPasswordRequest, LoginPasswordRequest, RevokeTokenRequest,
    SetupPasswordRequest,
};

use crate::format::{Classified, ExitCode};
use crate::session::{self, CachedSession};

#[derive(Debug, Subcommand)]
pub enum AuthAction {
    /// Set or change the password that gates access to rmail's own API
    /// (`ClientAuthService.SetupPassword`). Prompts twice, masked; the two
    /// must match.
    Setup,
    /// Remove the password gate entirely (`ClientAuthService.ClearPassword`).
    /// Also forgets any session this socket had cached — a cleared password
    /// makes it moot.
    Clear,
    /// Prove the password and cache a session token
    /// (`ClientAuthService.LoginPassword`), so later commands do not need
    /// `--token`.
    Login,
    /// End the cached session: revoke its token at the daemon
    /// (`AdminService.RevokeToken`) and forget it locally. Safe to run with
    /// nothing cached.
    Logout,
    /// Whether a password is configured, and whether local callers must log
    /// in too (`ClientAuthService.AuthStatus`).
    Status,
}

pub async fn run(socket: &Path, action: AuthAction) -> Result<()> {
    // `Login`/`Logout`/`Clear` all touch the *local* session cache
    // (`crate::session`, keyed by `socket` — the local `--socket`/
    // `$RMAIL_SOCKET` path, unconditionally, since the cache has no notion
    // of `--addr`) alongside an RPC that, under `--addr`, would go to a
    // remote daemon instead. Letting that combination through would mean
    // `login` caches a *remote* daemon's token under the local socket's key
    // (and `client::connect_parts` would then hand that token to the local
    // daemon on every later command), and `logout`/`clear` would revoke or
    // clear state belonging to whichever daemon `socket` happens to name —
    // unrelated to the one `--addr` just talked to. `Setup`/`Status` touch no
    // local state and are fine remotely.
    if matches!(
        action,
        AuthAction::Login | AuthAction::Logout | AuthAction::Clear
    ) {
        if let Some(addr) = crate::client::remote_addr() {
            return Err(Classified::new(
                ExitCode::Usage,
                format!(
                    "`mail auth login/logout/clear` manage the *local* session cache, which is \
                     keyed by socket path and has no `--addr` form — they cannot be pointed at \
                     --addr {addr}. Pass --token/$RMAIL_TOKEN for a remote daemon instead."
                ),
            ));
        }
    }
    match action {
        AuthAction::Setup => setup(socket).await,
        AuthAction::Clear => clear(socket).await,
        AuthAction::Login => login(socket).await,
        AuthAction::Logout => logout(socket).await,
        AuthAction::Status => status(socket).await,
    }
}

async fn setup(socket: &Path) -> Result<()> {
    let channel = crate::client::connect(socket).await?;
    let mut client = ClientAuthServiceClient::new(channel);

    // A password already configured means SetupPassword will refuse without
    // proof the caller knows it — see SetupPasswordRequest.current_password's
    // own doc for why. Asked up front rather than letting the RPC fail first,
    // so a caller just changing their password is not surprised by a second
    // prompt appearing after the new one is already typed twice.
    let already_configured = client
        .auth_status(AuthStatusRequest {})
        .await
        .context("AuthStatus RPC failed")?
        .into_inner()
        .password_configured;
    let current_password = if already_configured {
        rpassword::prompt_password("current rmail password: ")
            .context("reading password from stdin")?
    } else {
        String::new()
    };

    let password = prompt_new_password()?;
    client
        .setup_password(SetupPasswordRequest {
            password,
            current_password,
        })
        .await
        .context("SetupPassword RPC failed")?;
    println!("password set. Run `mail auth login` to start a session.");
    Ok(())
}

async fn clear(socket: &Path) -> Result<()> {
    let channel = crate::client::connect(socket).await?;
    let mut client = ClientAuthServiceClient::new(channel);
    client
        .clear_password(ClearPasswordRequest {})
        .await
        .context("ClearPassword RPC failed")?;
    // Best-effort: a cleared password makes any cached session for this
    // socket moot, but failing to forget it locally should not make `mail
    // auth clear` itself fail — the password is already gone at the daemon,
    // which is the part that actually matters.
    let _ = session::clear(socket);
    println!("password cleared.");
    Ok(())
}

async fn login(socket: &Path) -> Result<()> {
    let password =
        rpassword::prompt_password("rmail password: ").context("reading password from stdin")?;
    let channel = crate::client::connect(socket).await?;
    let mut client = ClientAuthServiceClient::new(channel);
    let response = client
        .login_password(LoginPasswordRequest { password })
        .await
        .context("LoginPassword RPC failed")?
        .into_inner();

    let cached = CachedSession {
        token: response.token.clone(),
        expires_at: response.expires_at,
        token_id: response.id,
    };
    match session::save(socket, &cached) {
        Ok(()) => println!("logged in — session cached, no --token needed for later commands."),
        Err(error) => {
            println!("logged in, but could not cache the session: {error}");
            println!();
            println!("token:   {}", response.token);
            println!();
            println!(
                "Store this now — it will not be shown again. Export it as $RMAIL_TOKEN, or \
                 pass --token, on every command until you log in again. `mail token revoke {}` \
                 ends it early.",
                response.id
            );
        }
    }
    Ok(())
}

async fn logout(socket: &Path) -> Result<()> {
    let Some(cached) = session::load(socket) else {
        println!("nothing cached for this socket.");
        return Ok(());
    };

    let channel = crate::client::connect(socket).await?;
    let mut admin = AdminServiceClient::new(channel);
    let revoked = admin
        .revoke_token(RevokeTokenRequest {
            id: cached.token_id,
        })
        .await;

    match revoked {
        Ok(_) => println!("revoked token {}.", cached.token_id),
        // `RevokeToken` is documented idempotent — NOT_FOUND is the one code
        // it returns for "there was never anything to revoke" (an id that
        // never existed at all), which is as good as done from here.
        Err(status) if status.code() == tonic::Code::NotFound => println!(
            "token {} was already gone at the daemon; forgetting the local session.",
            cached.token_id
        ),
        // Anything else (Unavailable, DeadlineExceeded, an auth failure...)
        // means the token may well still be live and revocable. Clearing the
        // cache here would lose the only local record of its id, so this
        // returns an error and leaves the cache in place instead — exit
        // non-zero, and a re-run of `mail auth logout` can pick the same
        // token_id back up and actually finish revoking it.
        Err(status) => {
            return Err(status).with_context(|| {
                format!(
                    "RevokeToken RPC failed for token {}; the session is still cached so a \
                     re-run can retry",
                    cached.token_id
                )
            });
        }
    }

    if let Err(error) = session::clear(socket) {
        println!("revoked at the daemon, but could not forget the cached session locally: {error}");
    } else {
        println!("logged out.");
    }
    Ok(())
}

async fn status(socket: &Path) -> Result<()> {
    let channel = crate::client::connect(socket).await?;
    let mut client = ClientAuthServiceClient::new(channel);
    let response = client
        .auth_status(AuthStatusRequest {})
        .await
        .context("AuthStatus RPC failed")?
        .into_inner();

    if crate::format::emit_response(rmail_core::parity::Command::ClientAuthAuthStatus, &response)? {
        return Ok(());
    }

    println!("password configured:  {}", response.password_configured);
    println!("local login required: {}", response.local_login_required);
    Ok(())
}

/// Prompt for a new password twice, masked, and require the two to match —
/// the same "type it again to confirm" every password-setting UI uses,
/// because a masked field cannot be proofread the way a visible one can.
fn prompt_new_password() -> Result<String> {
    let first = rpassword::prompt_password("new rmail password: ")
        .context("reading password from stdin")?;
    if first.is_empty() {
        anyhow::bail!("password must not be empty");
    }
    let second = rpassword::prompt_password("confirm: ").context("reading password from stdin")?;
    if first != second {
        anyhow::bail!("passwords did not match");
    }
    Ok(first)
}
