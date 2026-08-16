//! The `AccountService` gRPC implementation.
//!
//! `Create`/`List`/`Get`/`Delete` are wired to the account CRUD in
//! `rmail_core`; `TestConnection` logs in over IMAP and discovers folders.
//! Domain errors map to `tonic::Status` at this boundary.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmail_core::account::{self, NewAccount};
use rmail_core::autoconfig::{AutoconfigRequest, Autoconfigurator, ServerSettings};
use rmail_core::oauth::{OAuthBroker, PendingAuthorization, Provider};
use rmail_core::{CredentialSource, Database, Error};
use rmail_proto::v1::account_service_server::AccountService;
use rmail_proto::v1::{
    credential_ref, Account as ProtoAccount, AutoconfigureRequest, AutoconfigureResponse,
    BeginOAuthRequest, BeginOAuthResponse, CompleteOAuthRequest, CompleteOAuthResponse,
    CreateAccountRequest, CredentialRef, DeleteAccountRequest, DeleteAccountResponse,
    DiscoveredServer, GetAccountRequest, ListAccountsRequest, ListAccountsResponse,
    RefreshTokenRequest, RefreshTokenResponse, TestConnectionRequest, TestConnectionResponse,
};
use tonic::{Request, Response, Status};

/// The ceiling on one `Autoconfigure` call.
///
/// A discovery is a sequence of network round trips to hosts named by the
/// address under configuration: up to three documents, four SRV lookups and
/// an MX lookup at `probe`'s own per-request timeout, then an IMAP login at
/// `imap::IMAP_DEADLINE`. Each step is bounded; nothing bounded their sum,
/// and the sum is what an operator waits and what a caller can hold this
/// daemon's task for. Generous enough that a slow-but-working provider still
/// answers, short enough that it is not a way to pin resources.
const AUTOCONFIGURE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Most authorizations that may be in flight at once.
///
/// Each one holds a bound loopback port and a PKCE verifier until it is
/// completed or expires, and expiry is only noticed when another OAuth call
/// comes in. A caller that starts flows and never finishes them would
/// otherwise exhaust the ephemeral port range one `BeginOAuth` at a time.
/// Authorizing an account is a thing a human does a handful of times, so any
/// small number is generous.
const MAX_PENDING_FLOWS: usize = 8;

/// An authorization that has been started and not yet completed.
///
/// Held in memory rather than in the database on purpose: it owns a bound
/// loopback socket and a PKCE verifier, neither of which survives a restart
/// and neither of which belongs in a file. A daemon that restarts mid-flow
/// leaves the user with a browser tab that no longer has a listener — which is
/// exactly right, because the port is gone too.
struct PendingFlow {
    account_id: i64,
    /// `None` while `BeginOAuth` is still building the authorization.
    ///
    /// The entry exists from the moment the slot is claimed so that the cap is
    /// enforced against concurrent callers, and a slot in this state is not
    /// yet completable — [`AccountApi::complete_o_auth`] treats it as absent.
    authorization: Option<PendingAuthorization>,
    expires_at: i64,
}

/// A claimed slot in the pending-flow map, released on drop unless fulfilled.
///
/// The cap has to be enforced in the same lock acquisition that counts the
/// map, and `BeginOAuth` binds its loopback port on an `await` after that
/// point — so the slot is claimed first and filled in afterwards. Drop is what
/// makes every early return between the two release it, including the `?` on
/// `broker.begin`.
struct Reservation {
    flows: Arc<Mutex<HashMap<String, PendingFlow>>>,
    flow_id: String,
    fulfilled: bool,
}

impl Reservation {
    /// Prune expired flows and claim a slot, or fail if the cap is reached.
    fn claim(
        flows: Arc<Mutex<HashMap<String, PendingFlow>>>,
        flow_id: String,
        account_id: i64,
        expires_at: i64,
        now: i64,
    ) -> Result<Self, Status> {
        {
            let mut guard = flows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.retain(|_, flow| flow.expires_at > now);
            if guard.len() >= MAX_PENDING_FLOWS {
                return Err(Status::from(Error::resource_exhausted(format!(
                    "{MAX_PENDING_FLOWS} OAuth authorizations are already waiting for a \
                     browser; finish or abandon one (they expire on their own) before \
                     starting another"
                ))));
            }
            guard.insert(
                flow_id.clone(),
                PendingFlow {
                    account_id,
                    authorization: None,
                    expires_at,
                },
            );
        }
        Ok(Self {
            flows,
            flow_id,
            fulfilled: false,
        })
    }

    /// Fill the claimed slot in, so it survives this value being dropped.
    fn fulfil(mut self, authorization: PendingAuthorization) {
        if let Some(flow) = self
            .flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.flow_id)
        {
            flow.authorization = Some(authorization);
            self.fulfilled = true;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.fulfilled {
            self.flows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.flow_id);
        }
    }
}

/// The `AccountService` handler, backed by the local database.
#[derive(Clone)]
pub struct AccountApi {
    db: Database,
    broker: Arc<OAuthBroker>,
    /// In-flight authorizations, keyed by the opaque handle `BeginOAuth`
    /// returned.
    flows: Arc<Mutex<HashMap<String, PendingFlow>>>,
    /// Fires when the daemon is shutting down.
    ///
    /// `CompleteOAuth` waits up to five minutes for a human to finish
    /// consenting; without this, a shutdown would have to wait for that human
    /// too. Client disconnection needs no token — tonic drops the handler
    /// future, which drops the wait with it — so this covers the one case that
    /// dropping the future does not.
    stopping: tokio_util::sync::CancellationToken,
    /// The autoconfiguration engine (task 80).
    ///
    /// `None` on a daemon whose HTTP client could not be built, in which case
    /// `Autoconfigure` declines with `FAILED_PRECONDITION` rather than the
    /// RPC disappearing: reflection and the fail-closed scope table must see
    /// every RPC regardless of runtime configuration — the convention
    /// `AnalyticsService`/`AiService` established.
    autoconfig: Option<Autoconfigurator>,
}

impl AccountApi {
    /// Create a handler over the given database.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the process's OAuth broker cannot be
    /// built.
    pub fn new(db: Database, stopping: tokio_util::sync::CancellationToken) -> Result<Self, Error> {
        Ok(Self {
            db,
            broker: rmail_core::oauth::broker()?,
            flows: Arc::new(Mutex::new(HashMap::new())),
            stopping,
            autoconfig: None,
        })
    }

    /// Serve `Autoconfigure` from `engine`.
    #[must_use]
    pub fn with_autoconfig(mut self, engine: Autoconfigurator) -> Self {
        self.autoconfig = Some(engine);
        self
    }

    fn flows(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingFlow>> {
        self.flows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drop flows nobody came back for, so an abandoned authorization does not
    /// hold its loopback port (and its verifier) for the life of the daemon.
    fn expire_flows(&self, now: i64) {
        self.flows().retain(|_, flow| flow.expires_at > now);
    }

    /// The broker key for an OAuth account, or a `Status` explaining why there
    /// isn't one.
    async fn oauth_key(&self, account_id: i64) -> Result<rmail_core::oauth::StoreKey, Status> {
        let account = account::get(&self.db, account_id).await?;
        Ok(rmail_core::oauth::key_for(&account)?)
    }
}

#[tonic::async_trait]
impl AccountService for AccountApi {
    async fn create(
        &self,
        request: Request<CreateAccountRequest>,
    ) -> Result<Response<ProtoAccount>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.name.as_str());
        let new = NewAccount {
            name: req.name,
            imap_server: req.imap_server,
            imap_port: port_from_proto(req.imap_port)?,
            username: req.username,
            smtp_server: req.smtp_server,
            smtp_port: port_from_proto(req.smtp_port)?,
            credential: credential_from_proto(req.credential),
        };
        let account = account::create(&self.db, new).await?;
        Ok(Response::new(to_proto(&account)))
    }

    async fn list(
        &self,
        _request: Request<ListAccountsRequest>,
    ) -> Result<Response<ListAccountsResponse>, Status> {
        let accounts = account::list(&self.db).await?;
        Ok(Response::new(ListAccountsResponse {
            accounts: accounts.iter().map(to_proto).collect(),
        }))
    }

    async fn get(
        &self,
        request: Request<GetAccountRequest>,
    ) -> Result<Response<ProtoAccount>, Status> {
        let id = request.into_inner().id;
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, id);
        let account = account::get(&self.db, id).await?;
        Ok(Response::new(to_proto(&account)))
    }

    async fn delete(
        &self,
        request: Request<DeleteAccountRequest>,
    ) -> Result<Response<DeleteAccountResponse>, Status> {
        let id = request.into_inner().id;
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, id);
        account::delete(&self.db, id).await?;
        Ok(Response::new(DeleteAccountResponse { deleted: true }))
    }

    async fn test_connection(
        &self,
        request: Request<TestConnectionRequest>,
    ) -> Result<Response<TestConnectionResponse>, Status> {
        let id = request.into_inner().id;
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, id);
        let report = rmail_core::imap::test_connection(&self.db, id).await?;
        let caps = report.capabilities;
        let detail = format!(
            "connected; {} folders; capabilities: idle={} condstore={} qresync={} move={}",
            report.folders.len(),
            caps.idle,
            caps.condstore,
            caps.qresync,
            caps.move_,
        );
        Ok(Response::new(TestConnectionResponse { ok: true, detail }))
    }

    /// Discover settings for an address and return them as a proposal.
    ///
    /// Writes nothing — see [`rmail_core::autoconfig`]'s module docs. The
    /// handler's own job is the boundary translation: the request's
    /// `CredentialRef` becomes a [`CredentialSource`] (a reference, never a
    /// secret), and the daemon's shutdown token bounds the probes so a
    /// discovery against an unresponsive server cannot hold shutdown open.
    ///
    /// The whole call is bounded by [`AUTOCONFIGURE_DEADLINE`]. Each probe
    /// already has its own timeout, but they run in sequence and there are up
    /// to eight of them plus an IMAP login, so the *sum* is what an unlucky
    /// caller waits and what this daemon holds a task for — every one of those
    /// timeouts is against a host the request named.
    #[tracing::instrument(skip(self, request), fields(domain))]
    async fn autoconfigure(
        &self,
        request: Request<AutoconfigureRequest>,
    ) -> Result<Response<AutoconfigureResponse>, Status> {
        let req = request.into_inner();
        let Some(engine) = &self.autoconfig else {
            return Err(Status::from(Error::failed_precondition(
                "autoconfiguration is unavailable on this daemon: its HTTP client could not \
                 be built",
            )));
        };
        // The address is logged by domain only: the local part is the user's
        // identity and does not belong in a span field that ends up in logs.
        if let Some((_, domain)) = req.email.split_once('@') {
            tracing::Span::current().record("domain", domain);
        }
        let proposal = tokio::time::timeout(
            AUTOCONFIGURE_DEADLINE,
            engine.discover(
                &AutoconfigRequest {
                    email: req.email,
                    credential: credential_from_proto(req.credential),
                    allow_model_fallback: req.allow_model_fallback,
                },
                &self.stopping,
            ),
        )
        .await
        .map_err(|_| {
            Status::from(Error::deadline_exceeded(
                "autoconfiguration did not finish within its deadline; the servers being \
                 probed are unresponsive",
            ))
        })??;
        Ok(Response::new(AutoconfigureResponse {
            source: proposal.source.as_str().to_owned(),
            imap: Some(server_to_proto(&proposal.imap)),
            smtp: proposal.smtp.as_ref().map(server_to_proto),
            toml: proposal.toml,
            login_validated: proposal.login_validated,
            validation_detail: proposal.validation_detail,
            existing_account_id: proposal.existing_account_id.unwrap_or(0),
            warnings: proposal.warnings,
        }))
    }

    /// Start an authorization. The response carries a URL and a handle — never
    /// a token, and never the `state` on its own.
    #[tracing::instrument(skip(self, request), fields(account_id, provider))]
    async fn begin_o_auth(
        &self,
        request: Request<BeginOAuthRequest>,
    ) -> Result<Response<BeginOAuthResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        let provider = Provider::parse(&req.provider)?;
        tracing::Span::current().record("provider", provider.as_str());

        // The account must exist before a browser is opened: discovering it
        // does not *after* the user has consented means a completed
        // authorization with nowhere to store it.
        let account = account::get(&self.db, req.account_id).await?;
        if account.username.as_deref().unwrap_or_default().is_empty() {
            return Err(Status::from(Error::failed_precondition(
                "set the account's username (the mailbox address) before authorizing; \
                 an OAuth grant is stored per login",
            )));
        }

        // Resolved from a command, never carried over the wire, and resolved
        // *here* so a bad command fails before the user is sent to a browser.
        let client_secret = match req.client_secret_command.as_deref() {
            Some(command) if !command.trim().is_empty() => {
                let source = CredentialSource::Command(command.to_owned());
                tokio::task::spawn_blocking(move || source.resolve(None))
                    .await
                    .map_err(|e| {
                        Status::from(Error::internal(format!(
                            "client secret command task failed: {e}"
                        )))
                    })??
            }
            _ => None,
        };

        // The slot is taken *before* the await that binds the port, in the
        // same lock acquisition that counts them. Checking and then awaiting
        // would let N concurrent callers all observe an empty map, all bind a
        // port, and all insert — which is not a cap at all.
        let now = rmail_core::oauth::unix_now();
        let expires_at = now.saturating_add(
            i64::try_from(rmail_core::oauth::AUTHORIZATION_TIMEOUT.as_secs()).unwrap_or(i64::MAX),
        );
        let flow_id = new_flow_id();
        let reservation = Reservation::claim(
            Arc::clone(&self.flows),
            flow_id.clone(),
            req.account_id,
            expires_at,
            now,
        )?;

        let authorization = self
            .broker
            .begin(provider, &req.client_id, client_secret, Some(req.scopes))
            .await?;

        let response = BeginOAuthResponse {
            authorization_url: authorization.authorization_url(),
            redirect_uri: authorization.redirect_uri().to_owned(),
            flow_id,
            expires_at,
        };
        // Dropping the reservation without this releases the slot, which is
        // what must happen on every `?` above.
        reservation.fulfil(authorization);
        // The URL is *not* logged: it carries the `state` this flow will
        // accept a code against.
        tracing::info!(
            provider = provider.as_str(),
            redirect_uri = response.redirect_uri,
            "started an OAuth authorization"
        );
        Ok(Response::new(response))
    }

    /// Wait for the redirect and exchange the code.
    ///
    /// Long-running by nature — it is waiting for a human — so it honours the
    /// request's cancellation: a client that disconnects or times out releases
    /// the loopback port instead of leaving it bound for the full window.
    #[tracing::instrument(skip(self, request), fields(account_id))]
    async fn complete_o_auth(
        &self,
        request: Request<CompleteOAuthRequest>,
    ) -> Result<Response<CompleteOAuthResponse>, Status> {
        let flow_id = request.into_inner().flow_id;

        self.expire_flows(rmail_core::oauth::unix_now());
        // Removed rather than borrowed: an authorization code may be exchanged
        // exactly once, so a second `CompleteOAuth` on the same handle must
        // find nothing rather than race the first for the same socket.
        let flow = self
            .flows()
            .remove(&flow_id)
            // A slot whose authorization is still `None` belongs to a
            // `BeginOAuth` that has not returned yet; its handle cannot have
            // reached a caller, so this can only be a guess.
            .and_then(|flow| {
                flow.authorization
                    .map(|authorization| (flow.account_id, authorization))
            })
            .ok_or_else(|| {
                Error::not_found(
                    "no such OAuth flow; it was already completed, or it expired — \
                     start again with BeginOAuth",
                )
            })?;
        let (flow_account_id, flow_authorization) = flow;
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, flow_account_id);

        let account = account::get(&self.db, flow_account_id).await?;
        let provider = flow_authorization.provider();
        let service = oauth_service_name(&account, provider);
        // Re-checked, not assumed from `BeginOAuth`: the account may have been
        // edited while the user was in the browser. Storing under an empty
        // account field would write a live refresh token to a Keychain item
        // `oauth::key_for` can never address, and `set_credential` would then
        // reject the account anyway — leaving an orphaned credential behind a
        // failed RPC.
        let username = account.username.clone().unwrap_or_default();
        if username.is_empty() {
            return Err(Status::from(Error::failed_precondition(
                "the account lost its username while the authorization was in progress; \
                 set it and authorize again",
            )));
        }
        let key = rmail_core::oauth::StoreKey::new(service.clone(), username);

        let status = self
            .broker
            .complete(&key, flow_authorization, self.stopping.clone())
            .await?;

        // Only once the grant is safely stored: an account pointed at a
        // Keychain item that does not exist cannot sync at all, whereas an
        // account still on its old credential merely did not gain OAuth.
        account::set_credential(&self.db, flow_account_id, &CredentialSource::OAuth(service))
            .await?;

        Ok(Response::new(CompleteOAuthResponse {
            account_id: flow_account_id,
            provider: status.provider.as_str().to_owned(),
            expires_at: status.expires_at,
            scopes: status.scopes,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id))]
    async fn refresh_token(
        &self,
        request: Request<RefreshTokenRequest>,
    ) -> Result<Response<RefreshTokenResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        let key = self.oauth_key(req.account_id).await?;
        let status = self.broker.refresh(&key, req.force).await?;
        Ok(Response::new(RefreshTokenResponse {
            expires_at: status.expires_at,
            refreshed: status.refreshed,
            provider: status.provider.as_str().to_owned(),
            scopes: status.scopes,
        }))
    }
}

/// The Keychain service an account's OAuth grant is filed under.
///
/// Derived from the account id rather than from its name, because a name is
/// editable and a Keychain item renamed out from under a grant is a grant
/// nothing can find. Reuses an existing OAuth `secret_ref` so re-authorizing
/// overwrites the item already there instead of orphaning it.
fn oauth_service_name(account: &account::Account, provider: Provider) -> String {
    match &account.credential {
        CredentialSource::OAuth(service) => service.clone(),
        _ => format!("rmail-oauth-{}-{}", provider.as_str(), account.id),
    }
}

/// An opaque handle for an in-flight authorization.
///
/// Unguessable, because holding one is what lets a caller consume the code the
/// browser is about to deliver.
fn new_flow_id() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Convert a proto port (`uint32`) to the domain `u16`, requiring `1..=65535`.
fn port_from_proto(port: Option<u32>) -> Result<Option<u16>, Status> {
    match port {
        None => Ok(None),
        Some(0) => Err(Status::from(Error::invalid_argument(
            "port must be in 1..=65535",
        ))),
        Some(p) => u16::try_from(p)
            .map(Some)
            .map_err(|_| Status::from(Error::invalid_argument("port must be in 1..=65535"))),
    }
}

/// Build a [`CredentialSource`] from the optional proto credential reference.
fn credential_from_proto(credential: Option<CredentialRef>) -> CredentialSource {
    match credential.and_then(|c| c.source) {
        Some(credential_ref::Source::PasswordCommand(cmd)) => CredentialSource::Command(cmd),
        Some(credential_ref::Source::PasswordEnv(var)) => CredentialSource::Env(var),
        Some(credential_ref::Source::Keychain(service)) => CredentialSource::Keychain(service),
        Some(credential_ref::Source::Oauth(service)) => CredentialSource::OAuth(service),
        None => CredentialSource::None,
    }
}

/// Project a validated discovered server onto the wire.
fn server_to_proto(settings: &ServerSettings) -> DiscoveredServer {
    DiscoveredServer {
        host: settings.host.clone(),
        port: u32::from(settings.port),
        security: settings.security.as_str().to_owned(),
        username: settings.username.clone(),
    }
}

/// Project a domain account onto its proto representation (never the secret).
fn to_proto(account: &account::Account) -> ProtoAccount {
    ProtoAccount {
        id: account.id,
        name: account.name.clone(),
        imap_server: account.imap_server.clone(),
        imap_port: account.imap_port.map(u32::from),
        username: account.username.clone(),
        smtp_server: account.smtp_server.clone(),
        smtp_port: account.smtp_port.map(u32::from),
        credential_kind: account.credential.kind().to_owned(),
        credential_ref: account.credential.reference().map(str::to_owned),
        created_at: account.created_at,
        updated_at: account.updated_at,
    }
}
