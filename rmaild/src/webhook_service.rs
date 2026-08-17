//! The `WebhookService` gRPC implementation (task 68, prd.md #49 and #64).
//!
//! Thin, like `ExtractService`: [`rmail_core::webhooks`] owns the URL policy,
//! the redaction, the signing, the queue and the retry machinery, and what
//! lives here is the translation between its types and the wire — plus the
//! three decisions that are genuinely a transport concern.
//!
//! # Why this service has real CRUD when `HookService` does not
//!
//! `HookService` is read-only because a hook is a line in the operator's TOML
//! file and `mail hook add` edits that file directly (see `rmail-cli::hook_cli`
//! for why). A webhook destination cannot work that way: it carries live
//! per-destination state — a delivery queue, an attempt history, a terminal
//! failure an operator has to replay out of — and it is added and removed
//! while the daemon runs. So destinations are rows (V48), not TOML, and
//! `Register`/`Remove` are genuine mutations rather than config-file edits
//! pretending to be RPCs. `[webhooks]` in the TOML keeps only the dispatcher's
//! own dials, which is exactly the split
//! `rmail_core::config::WebhooksConfig`'s own docs describe.
//!
//! # A caller names a *destination*, never a URL or a body policy
//!
//! `Forward` takes a destination name. It cannot supply a URL — a caller that
//! could would have an outbound request generator, the same hazard
//! `ExtractService` documents for its sinks — and it cannot ask for a message
//! body, because `include_body` is a property of the destination the operator
//! registered. The one place a URL enters the system is `Register`, which is
//! gated on `automation` and validated by
//! [`rmail_core::webhooks::validate_url`] before anything is stored.
//!
//! # `Register` is `automation` + `mail.read`, and the second half is not
//! # decoration
//!
//! Registering a destination does not itself read any mail — but it is the act
//! that causes mail to be read and shipped, on every future event, to an
//! address the caller chose. Gating it on `automation` alone would let a token
//! that is forbidden from reading a single message arrange for every message
//! to be POSTed somewhere it can read. `rmaild::auth::methods` therefore
//! requires both, the same `AllOf` shape and the same reasoning
//! `ExtractService/ExtractEvents` carries.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every
// `Result<_, Status>` helper, so the lint is allowed for this module — the
// same allowance `extract_service.rs`/`audit_service.rs` carry.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use rmail_core::config::{AiPrivacy, HookEvent};
use rmail_core::credential::CredentialSource;
use rmail_core::storage::Database;
use rmail_core::webhooks::{self, store, Delivery, DeliveryState, Destination, Template};
use rmail_core::Error;
use rmail_proto::v1::webhook_service_server::WebhookService;
use rmail_proto::v1::{
    ForwardMessageRequest, ForwardMessageResponse, ListDeliveriesRequest, ListDeliveriesResponse,
    ListWebhooksRequest, ListWebhooksResponse, RegisterWebhookRequest, RegisterWebhookResponse,
    RemoveWebhookRequest, RemoveWebhookResponse, ReplayDeliveryRequest, ReplayDeliveryResponse,
    SetWebhookEnabledRequest, SetWebhookEnabledResponse, WebhookDelivery, WebhookDeliveryState,
    WebhookDestination, WebhookEvent, WebhookSecretSource, WebhookTemplate,
};
use tonic::{Request, Response, Status};

/// Default `ListDeliveries` page size.
const DEFAULT_DELIVERY_LIMIT: i64 = 50;

/// The `WebhookService` handler.
#[derive(Debug, Clone)]
pub struct WebhookApi {
    db: Database,
    /// Whether this daemon runs a dispatcher (`webhooks.enabled`).
    ///
    /// Held only so `Forward` can *say* so. It deliberately does not gate any
    /// RPC here: registering, listing and removing destinations has to work
    /// before an operator has ever switched the dispatcher on, or the feature
    /// is unconfigurable until somebody enables it blind.
    dispatcher_running: bool,
    /// The operator's own `[ai.privacy]` settings — the redaction firewall
    /// applied to every payload built here. Held rather than re-read per call
    /// for the same reason every other service holds its config: a request
    /// must not be able to observe a different policy than the one the daemon
    /// booted with.
    privacy: AiPrivacy,
}

impl WebhookApi {
    /// Build a handler over `db`.
    #[must_use]
    pub fn new(db: Database, privacy: AiPrivacy, dispatcher_running: bool) -> Self {
        Self {
            db,
            privacy,
            dispatcher_running,
        }
    }

    /// Resolve `name` to a destination id, for the listing filter.
    async fn destination_id(&self, name: &str) -> Result<Option<i64>, Status> {
        if name.trim().is_empty() {
            return Ok(None);
        }
        let destination = store::get_by_name(&self.db, name.trim())
            .await
            .map_err(Status::from)?;
        Ok(Some(destination.id))
    }

    /// The `id -> name` map `ListDeliveries` needs to name each row's
    /// destination. One query for the whole page rather than one per row.
    async fn destination_names(&self) -> Result<HashMap<i64, String>, Status> {
        Ok(store::list(&self.db)
            .await
            .map_err(Status::from)?
            .into_iter()
            .map(|d| (d.id, d.name))
            .collect())
    }
}

#[tonic::async_trait]
impl WebhookService for WebhookApi {
    #[tracing::instrument(skip(self, request), fields(name, template, events, include_body))]
    async fn register(
        &self,
        request: Request<RegisterWebhookRequest>,
    ) -> Result<Response<RegisterWebhookResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("name", req.name.as_str());
        span.record("include_body", req.include_body);
        // The URL is deliberately *not* recorded on this span: a Slack
        // incoming-webhook URL is itself a bearer credential. See
        // `webhooks::log_url`.

        let template = template_from_proto(req.template);
        let events = events_from_proto(&req.events)?;
        span.record("template", template.as_str());
        span.record("events", events.len());

        let secret = secret_from_proto(req.secret_source, &req.secret_reference)?;
        let new = webhooks::NewDestination {
            name: req.name,
            url: req.url,
            template,
            events,
            include_body: req.include_body,
            enabled: !req.disabled,
            secret,
            max_attempts: if req.max_attempts <= 0 {
                5
            } else {
                req.max_attempts
            },
        };
        let destination = store::register(&self.db, new).await.map_err(Status::from)?;
        Ok(Response::new(RegisterWebhookResponse {
            // Revealed: the caller supplied this URL in this very request, so
            // echoing it back discloses nothing it does not already hold.
            destination: Some(to_proto_destination(&destination, true)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(reveal_url, destinations))]
    async fn list(
        &self,
        request: Request<ListWebhooksRequest>,
    ) -> Result<Response<ListWebhooksResponse>, Status> {
        let reveal_url = request.into_inner().reveal_url;
        let span = tracing::Span::current();
        span.record("reveal_url", reveal_url);
        let destinations = store::list(&self.db).await.map_err(Status::from)?;
        span.record("destinations", destinations.len());
        Ok(Response::new(ListWebhooksResponse {
            destinations: destinations
                .iter()
                .map(|d| to_proto_destination(d, reveal_url))
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(name, enabled))]
    async fn set_enabled(
        &self,
        request: Request<SetWebhookEnabledRequest>,
    ) -> Result<Response<SetWebhookEnabledResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("name", req.name.as_str());
        span.record("enabled", req.enabled);
        let destination = store::set_enabled(&self.db, req.name.trim(), req.enabled)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(SetWebhookEnabledResponse {
            destination: Some(to_proto_destination(&destination, false)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(name, removed))]
    async fn remove(
        &self,
        request: Request<RemoveWebhookRequest>,
    ) -> Result<Response<RemoveWebhookResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("name", req.name.as_str());
        let removed = store::remove(&self.db, req.name.trim())
            .await
            .map_err(Status::from)?;
        tracing::Span::current().record("removed", removed);
        Ok(Response::new(RemoveWebhookResponse { removed }))
    }

    #[tracing::instrument(
        skip(self, request),
        fields(destination, limit, include_payload, deliveries)
    )]
    async fn list_deliveries(
        &self,
        request: Request<ListDeliveriesRequest>,
    ) -> Result<Response<ListDeliveriesResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("destination", req.destination.as_str());
        span.record("include_payload", req.include_payload);
        let destination_id = self.destination_id(&req.destination).await?;
        let limit = if req.limit <= 0 {
            DEFAULT_DELIVERY_LIMIT
        } else {
            req.limit
        };
        span.record("limit", limit);
        let deliveries = store::list_deliveries(&self.db, destination_id, limit)
            .await
            .map_err(Status::from)?;
        span.record("deliveries", deliveries.len());
        let names = self.destination_names().await?;
        Ok(Response::new(ListDeliveriesResponse {
            deliveries: deliveries
                .iter()
                .map(|d| to_proto_delivery(d, &names, req.include_payload))
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(delivery_id))]
    async fn replay_delivery(
        &self,
        request: Request<ReplayDeliveryRequest>,
    ) -> Result<Response<ReplayDeliveryResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("delivery_id", req.delivery_id);
        let delivery = store::replay(&self.db, req.delivery_id)
            .await
            .map_err(Status::from)?;
        let names = self.destination_names().await?;
        Ok(Response::new(ReplayDeliveryResponse {
            // Never with the payload: a replay's answer is "it is queued
            // again", and echoing the mail content back is not part of it.
            delivery: Some(to_proto_delivery(&delivery, &names, false)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(message_id, destination, delivery_id))]
    async fn forward(
        &self,
        request: Request<ForwardMessageRequest>,
    ) -> Result<Response<ForwardMessageResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("message_id", req.message_id);
        span.record("destination", req.destination.as_str());
        if req.message_id <= 0 {
            return Err(Status::from(Error::invalid_argument(
                "message_id is required".to_owned(),
            )));
        }
        let destination = req.destination.trim();
        if destination.is_empty() {
            return Err(Status::from(Error::invalid_argument(
                "a destination name is required (see `mail webhook list`)".to_owned(),
            )));
        }
        let now = chrono::Utc::now().timestamp();
        let id = webhooks::forward(&self.db, destination, req.message_id, &self.privacy, now)
            .await
            .map_err(Status::from)?;
        span.record("delivery_id", id);
        let delivery = store::get_delivery(&self.db, id)
            .await
            .map_err(Status::from)?;
        let names = self.destination_names().await?;
        Ok(Response::new(ForwardMessageResponse {
            delivery: Some(to_proto_delivery(&delivery, &names, false)),
            dispatcher_running: self.dispatcher_running,
        }))
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn to_proto_destination(destination: &Destination, reveal_url: bool) -> WebhookDestination {
    WebhookDestination {
        id: destination.id,
        name: destination.name.clone(),
        url: if reveal_url {
            destination.url.clone()
        } else {
            // The same reduction `webhooks::log_url` performs for a log line,
            // and for the same reason — see `WebhookDestination.url` in the
            // proto.
            webhooks::log_url(&destination.url)
        },
        template: template_to_proto(destination.template) as i32,
        events: destination
            .events
            .iter()
            .map(|e| event_to_proto(*e) as i32)
            .collect(),
        include_body: destination.include_body,
        enabled: destination.enabled,
        secret_source: secret_to_proto(&destination.secret) as i32,
        secret_reference: destination
            .secret
            .reference()
            .unwrap_or_default()
            .to_owned(),
        max_attempts: destination.max_attempts,
    }
}

fn to_proto_delivery(
    delivery: &Delivery,
    names: &HashMap<i64, String>,
    include_payload: bool,
) -> WebhookDelivery {
    WebhookDelivery {
        id: delivery.id,
        destination_id: delivery.destination_id,
        destination_name: names
            .get(&delivery.destination_id)
            .cloned()
            .unwrap_or_default(),
        event_key: delivery.event_key.clone(),
        event: delivery.event.clone(),
        message_id: delivery.message_id.unwrap_or_default(),
        state: state_to_proto(delivery.state) as i32,
        attempts: delivery.attempts,
        max_attempts: delivery.max_attempts,
        next_attempt_at: delivery.next_attempt_at.unwrap_or_default(),
        // Saturating rather than wrapping: an out-of-range status is a
        // corrupt row, and reporting `0` ("the peer never answered") is a
        // lie an operator would act on, where a clamped value is visibly odd.
        last_status: i32::try_from(delivery.last_status.unwrap_or_default()).unwrap_or(i32::MAX),
        last_error: delivery.last_error.clone().unwrap_or_default(),
        created_at: delivery.created_at,
        delivered_at: delivery.delivered_at.unwrap_or_default(),
        payload: if include_payload {
            delivery.payload.clone()
        } else {
            String::new()
        },
    }
}

fn template_from_proto(value: i32) -> Template {
    match WebhookTemplate::try_from(value) {
        Ok(WebhookTemplate::Slack) => Template::Slack,
        // Unspecified is documented as GENERIC, and an unrecognized value
        // from a newer client degrades to the same rather than being refused:
        // the rendering is a presentation choice, not an authorization one.
        _ => Template::Generic,
    }
}

const fn template_to_proto(template: Template) -> WebhookTemplate {
    match template {
        Template::Generic => WebhookTemplate::Generic,
        Template::Slack => WebhookTemplate::Slack,
    }
}

/// Parse a subscription. An unspecified/unknown event is refused rather than
/// dropped — silently registering a destination that subscribes to less than
/// the caller asked for is how somebody discovers six months later that their
/// alerts never fired.
fn events_from_proto(events: &[i32]) -> Result<Vec<HookEvent>, Status> {
    let mut out = Vec::with_capacity(events.len());
    for value in events {
        let parsed = match WebhookEvent::try_from(*value) {
            Ok(WebhookEvent::OnNewMessage) => HookEvent::OnNewMessage,
            Ok(WebhookEvent::OnLabel) => HookEvent::OnLabel,
            Ok(WebhookEvent::OnMove) => HookEvent::OnMove,
            Ok(WebhookEvent::OnRuleMatch) => HookEvent::OnRuleMatch,
            Ok(WebhookEvent::OnSyncError) => HookEvent::OnSyncError,
            Ok(WebhookEvent::Unspecified) | Err(_) => {
                return Err(Status::from(Error::invalid_argument(format!(
                    "unknown webhook event {value}"
                ))));
            }
        };
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    Ok(out)
}

const fn event_to_proto(event: HookEvent) -> WebhookEvent {
    match event {
        HookEvent::OnNewMessage => WebhookEvent::OnNewMessage,
        HookEvent::OnLabel => WebhookEvent::OnLabel,
        HookEvent::OnMove => WebhookEvent::OnMove,
        HookEvent::OnRuleMatch => WebhookEvent::OnRuleMatch,
        HookEvent::OnSyncError => WebhookEvent::OnSyncError,
    }
}

const fn state_to_proto(state: DeliveryState) -> WebhookDeliveryState {
    match state {
        DeliveryState::Pending => WebhookDeliveryState::Pending,
        DeliveryState::Delivered => WebhookDeliveryState::Delivered,
        DeliveryState::Failed => WebhookDeliveryState::Failed,
    }
}

/// Turn `(source, reference)` into a [`CredentialSource`].
///
/// A source that names nowhere to look is refused here rather than stored: a
/// destination whose signature can never be produced would fail every delivery
/// with a credential error the operator only sees in the queue.
fn secret_from_proto(source: i32, reference: &str) -> Result<CredentialSource, Status> {
    let reference = reference.trim();
    let kind = WebhookSecretSource::try_from(source).unwrap_or(WebhookSecretSource::Unspecified);
    if matches!(kind, WebhookSecretSource::Unspecified) {
        return Ok(CredentialSource::None);
    }
    if reference.is_empty() {
        return Err(Status::from(Error::invalid_argument(
            "a signing-key source needs a reference (the env var name, the command, or the \
             keychain service)"
                .to_owned(),
        )));
    }
    Ok(match kind {
        WebhookSecretSource::Env => CredentialSource::Env(reference.to_owned()),
        WebhookSecretSource::Command => CredentialSource::Command(reference.to_owned()),
        WebhookSecretSource::Keychain => CredentialSource::Keychain(reference.to_owned()),
        WebhookSecretSource::Unspecified => CredentialSource::None,
    })
}

fn secret_to_proto(source: &CredentialSource) -> WebhookSecretSource {
    match source {
        CredentialSource::Env(_) => WebhookSecretSource::Env,
        CredentialSource::Command(_) => WebhookSecretSource::Command,
        CredentialSource::Keychain(_) => WebhookSecretSource::Keychain,
        // `OAuth` is refused at registration (`store::register`), so a stored
        // row can never carry it; reporting it as "unsigned" is the honest
        // answer for a row a future version wrote.
        CredentialSource::None | CredentialSource::OAuth(_) => WebhookSecretSource::Unspecified,
    }
}
