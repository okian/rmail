//! The `AccountService` gRPC implementation.
//!
//! `Create`/`List`/`Get`/`Delete` are wired to the account CRUD in
//! `rmail_core`; `TestConnection` returns `UNIMPLEMENTED` until IMAP login
//! lands (task 8). Domain errors map to `tonic::Status` at this boundary.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use rmail_core::account::{self, NewAccount};
use rmail_core::{CredentialSource, Database, Error};
use rmail_proto::v1::account_service_server::AccountService;
use rmail_proto::v1::{
    credential_ref, Account as ProtoAccount, CreateAccountRequest, CredentialRef,
    DeleteAccountRequest, DeleteAccountResponse, GetAccountRequest, ListAccountsRequest,
    ListAccountsResponse, TestConnectionRequest, TestConnectionResponse,
};
use tonic::{Request, Response, Status};

/// The `AccountService` handler, backed by the local database.
#[derive(Clone)]
pub struct AccountApi {
    db: Database,
}

impl AccountApi {
    /// Create a handler over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
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
        tracing::Span::current().record(
            rmail_core::telemetry::FIELD_ACCOUNT,
            request.into_inner().id,
        );
        Err(Status::unimplemented(
            "TestConnection requires IMAP login, which lands in task 8",
        ))
    }
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
        None => CredentialSource::None,
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
