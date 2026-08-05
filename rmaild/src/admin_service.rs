//! The `AdminService` gRPC implementation.
//!
//! `MintToken`/`RevokeToken`/`ListTokens` wrap [`rmail_core::auth`]'s token
//! lifecycle. There is no scope check in this file: the requirement (`admin`,
//! for every method here — see `rmaild::auth::methods`) is enforced once, for
//! every service, by the auth layer wrapping the whole server. A handler that
//! re-checked scope here would be a second source of truth to keep in sync
//! with the table, not a second line of defense.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use rmail_core::auth::{self, ApiToken, NewToken, Scope};
use rmail_core::{Database, Error};
use rmail_proto::v1::admin_service_server::AdminService;
use rmail_proto::v1::{
    ListTokensRequest, ListTokensResponse, MintTokenRequest, MintTokenResponse, RevokeTokenRequest,
    RevokeTokenResponse, TokenInfo,
};
use tonic::{Request, Response, Status};

/// The `AdminService` handler, backed by the local database.
#[derive(Clone)]
pub struct AdminApi {
    db: Database,
}

impl AdminApi {
    /// Create a handler over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[tonic::async_trait]
impl AdminService for AdminApi {
    async fn mint_token(
        &self,
        request: Request<MintTokenRequest>,
    ) -> Result<Response<MintTokenResponse>, Status> {
        let req = request.into_inner();
        let scopes = parse_scopes(&req.scopes)?;
        let minted = auth::mint(
            &self.db,
            NewToken {
                name: req.name,
                scopes,
                ttl_secs: req.ttl_secs,
            },
        )
        .await?;

        tracing::info!(
            token_id = minted.token.id,
            name = minted.token.name.as_str(),
            "minted capability token"
        );

        Ok(Response::new(MintTokenResponse {
            id: minted.token.id,
            token: minted.secret,
            name: minted.token.name,
            scopes: minted.token.scopes.iter().map(Scope::as_wire).collect(),
            created_at: minted.token.created_at,
            expires_at: minted.token.expires_at,
        }))
    }

    async fn revoke_token(
        &self,
        request: Request<RevokeTokenRequest>,
    ) -> Result<Response<RevokeTokenResponse>, Status> {
        let id = request.into_inner().id;
        auth::revoke(&self.db, id).await?;
        tracing::info!(token_id = id, "revoked capability token");
        Ok(Response::new(RevokeTokenResponse { revoked: true }))
    }

    async fn list_tokens(
        &self,
        _request: Request<ListTokensRequest>,
    ) -> Result<Response<ListTokensResponse>, Status> {
        let tokens = auth::list(&self.db).await?;
        Ok(Response::new(ListTokensResponse {
            tokens: tokens.iter().map(to_proto).collect(),
        }))
    }
}

/// Parse `MintTokenRequest.scopes`, mapping the first unparseable entry to
/// `INVALID_ARGUMENT` with the offending string named.
fn parse_scopes(scopes: &[String]) -> Result<Vec<Scope>, Status> {
    scopes
        .iter()
        .map(|s| {
            s.parse::<Scope>()
                .map_err(|e| Status::from(Error::invalid_argument(format!("scope {s:?}: {e}"))))
        })
        .collect()
}

/// Project a domain token onto its proto representation (never the hash).
fn to_proto(token: &ApiToken) -> TokenInfo {
    TokenInfo {
        id: token.id,
        name: token.name.clone(),
        scopes: token.scopes.iter().map(Scope::as_wire).collect(),
        created_at: token.created_at,
        last_used_at: token.last_used_at,
        expires_at: token.expires_at,
        revoked: token.revoked,
    }
}
