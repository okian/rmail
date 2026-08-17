//! Outbound webhooks (task 68, prd.md #49 "Outbound Webhooks with
//! AI-Enriched Payloads" and #64 "Slack/Chat Forwarding with AI Summary
//! Payloads").
//!
//! This is the *network* sibling of [`crate::hooks`], not a second event
//! system. Both consume the one durable event log ([`crate::events::EventLog`],
//! task 14) from their own in-memory cursor; both are bounded, cancellable and
//! incapable of stalling that log. The difference is the whole of this
//! module's risk surface: a hook runs a command the operator wrote, on this
//! machine, and a webhook puts the operator's mail on somebody else's server.
//! Everything below follows from that one sentence.
//!
//! # Nothing is sent anywhere the operator did not name
//!
//! There is no default destination and no implicit one. `webhooks.enabled` is
//! `false` by default, and even switched on it sends nothing until a row
//! exists in `webhook_destinations` — registered by `mail webhook add` or
//! `WebhookService/Register`, per destination, with its own event
//! subscription. A destination that subscribes to no events is legal and
//! useful: it receives explicit `mail forward` pushes and no firehose.
//!
//! Turning a destination off is [`store::set_enabled`], not [`store::remove`]:
//! "stop sending here" and "forget where this pointed" are different intents,
//! and only the second discards the delivery history. A disabled destination's
//! already-queued deliveries stay queued and stop being claimed
//! ([`store::claim_due`] filters on it, and [`WebhookDispatcher::attempt`]
//! re-checks it), so re-enabling resumes rather than replays.
//!
//! # The default payload is the least that makes a notification useful
//!
//! Sender, subject, message id, deep link, account/mailbox, date. Not the
//! body, not attachments, not recipients, not headers. `include_body` is a
//! per-destination column, off by default, and is the only way a body ever
//! leaves. [`payload`]'s own module docs are the field-by-field statement of
//! what each shape carries and why; that file is the one to read before
//! changing anything about what goes on the wire.
//!
//! Everything derived from message content is put through
//! [`crate::ai::redact`] — the same firewall, with the operator's same
//! `[ai.privacy]` settings, that governs text going to a model provider — with
//! one documented exemption for the sender address. See [`payload`].
//!
//! # AI enrichment is read, never computed
//!
//! A payload's `summary`/`action_items` come from `ai_summaries`, the
//! artifacts the triage and deep passes already stored. Building or retrying a
//! delivery never calls a provider. A dispatcher that could spend money per
//! inbound message would be a cost amplifier bolted to an attacker-controlled
//! trigger, and a *retry* that re-ran the model call would multiply that by
//! the attempt cap.
//!
//! # A destination URL is configuration, and is still treated defensively
//!
//! [`validate_url`] requires `https`, with plaintext allowed only for a
//! loopback host (which cannot leave the machine, and is what the tests
//! drive). It is enforced at registration *and* again immediately before every
//! POST, so a row written by a future version, a direct `sqlite3` edit, or a
//! restored backup cannot become an egress this build would not have accepted.
//!
//! Redirects are never followed. [`build_client`] sets
//! [`reqwest::redirect::Policy::none`], so a destination answering `302
//! Location: http://attacker.example/` gets one request to the URL the
//! operator registered and nothing else — the daemon is not walked to a second
//! host carrying mail content and a valid signature. Task 79 fixed exactly
//! this hazard on the OAuth token-refresh path; the reasoning is identical and
//! the stakes here are higher, because the body is the mail rather than a
//! grant. A redirect is treated as a *permanent* failure rather than a retried
//! one: a destination that redirects is misconfigured, and retrying will
//! redirect again.
//!
//! # Signing
//!
//! [`sign`] is HMAC-SHA256 over `<timestamp>.<body>`, sent as
//! `X-Rmail-Signature: v1=<hex>` beside `X-Rmail-Timestamp`. The key is a
//! [`crate::credential::Secret`] resolved through the existing provider
//! (`password_command`/env/keychain) — `webhook_destinations` stores the
//! *reference*, never the key, exactly as `accounts` does. Nothing in this
//! module logs the key, the payload, or the destination's URL path (a Slack
//! incoming-webhook URL is itself a bearer credential): see [`log_url`], which
//! is also what `WebhookService/List` reduces a URL to unless a caller
//! explicitly asks to see it. `Delivery` and [`ClaimedDelivery`] carry
//! hand-written `Debug` impls for the same reason — a derived one would put
//! the whole frozen payload and the full URL into any future
//! `tracing::debug!(?delivery, ..)`.
//!
//! Every timestamp a signature binds is taken at the moment its own request is
//! built, never once per batch — see [`WebhookDispatcher::attempt`] for what a
//! shared timestamp costs against a receiver enforcing a freshness window.
//!
//! # Delivery is idempotent per event, retried with backoff, and terminal
//!
//! One row per `(destination, event_key)` behind V48's UNIQUE index, enqueued
//! with `INSERT OR IGNORE`, so the database decides who was first and two
//! ticks racing the same event cannot both queue it. Each row carries its own
//! frozen body, its own attempt count and its own cap; a failure defers it
//! with exponential backoff, and a row whose attempts are spent moves to
//! `failed` and stops. `ReplayDelivery` is the only way back out, and it is
//! operator-driven on purpose — an unbounded automatic retry against a
//! misconfigured URL is an outbound request generator.
//!
//! # The cursor starts at "now," never at the beginning of retention
//!
//! [`WebhookDispatcher::spawn`] seeds its cursor from
//! [`crate::events::EventLog::latest_seq`] before returning, exactly as
//! [`crate::hooks::HookDispatcher`] does and for a closely related reason.
//! The UNIQUE index would make a replay a no-op for deliveries already
//! enqueued — but not for a destination registered *since*: replaying from `0`
//! would flood a brand-new endpoint with a week of history the moment it was
//! added. Seeding at the head means a destination starts receiving from when
//! it was registered, which is the only reading of "subscribe" anybody
//! expects.

pub mod payload;
pub mod sign;
pub mod store;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::config::{AiPrivacy, HookEvent, WebhooksConfig};
use crate::credential::{CredentialSource, Secret};
use crate::error::{Error, ErrorReason};
use crate::events::EventLog;
use crate::storage::Database;

pub use payload::{deep_link, MessageFacts};
pub use store::ClaimedDelivery;

/// How many durable-log events one [`WebhookDispatcher::tick`] page reads at a
/// time — the same value and reasoning as `hooks::DRAIN_PAGE`.
const DRAIN_PAGE: i64 = 500;

/// Default interval between dispatch ticks.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Floor on [`crate::config::WebhooksConfig::tick_interval`], so a `"0s"` typo
/// degrades to "as fast as is sane" rather than to a busy loop against the
/// event log and the delivery queue.
pub const MIN_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// The `event` value a manual forward is recorded under — deliberately not one
/// of [`HookEvent`]'s values, because a forward is a person pressing a button
/// rather than something that happened to the mailbox.
pub const FORWARD_EVENT: &str = "forward";

/// The `User-Agent` every outbound webhook identifies itself with. A receiver
/// that wants to allowlist rmail specifically has something stable to match,
/// and a receiver debugging an unexpected POST can tell what sent it.
const USER_AGENT: &str = concat!("rmail/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// How a destination's payload is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Template {
    /// A JSON document with the facts as named fields. The shape a ticketing
    /// system, an n8n flow or a bespoke receiver wants.
    #[default]
    Generic,
    /// Slack's incoming-webhook shape: a rendered `text` field (mrkdwn,
    /// escaped — see [`payload::slack_escape`]) with the structured object
    /// alongside it.
    Slack,
}

impl Template {
    /// Every template, for CLI/proto enumeration.
    pub const ALL: [Self; 2] = [Self::Generic, Self::Slack];

    /// The stable string stored in `webhook_destinations.template`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Slack => "slack",
        }
    }

    /// Parse the stored/wire form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "generic" => Some(Self::Generic),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }
}

/// Where one delivery has got to. Matches V48's `state` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Queued, or waiting out a backoff. The only state an attempt may start
    /// from.
    Pending,
    /// The endpoint answered 2xx. Terminal.
    Delivered,
    /// Attempts spent, or a refusal there is no point repeating. Terminal
    /// until [`store::replay`].
    Failed,
}

impl DeliveryState {
    /// The stable stored string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }

    /// Parse the stored/wire form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A registered endpoint.
///
/// `Debug` is derived and that is safe by construction: `secret` is a
/// [`CredentialSource`], which holds only a *reference* (a command line, an
/// env var name, a keychain service) and never the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Row id.
    pub id: i64,
    /// The operator's handle — what `mail forward --to <name>` names.
    pub name: String,
    /// Where the POST goes. Always re-checked by [`validate_url`] before use.
    pub url: String,
    /// How the payload is rendered.
    pub template: Template,
    /// The events this destination subscribes to. Empty is legal: an
    /// explicit-forward-only destination.
    pub events: Vec<HookEvent>,
    /// Whether this destination is entitled to message bodies.
    pub include_body: bool,
    /// Whether the dispatcher sends to it at all.
    pub enabled: bool,
    /// Where the signing key comes from. [`CredentialSource::None`] means
    /// requests are unsigned.
    pub secret: CredentialSource,
    /// The attempt cap stamped onto this destination's new deliveries.
    pub max_attempts: i64,
}

/// A destination to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDestination {
    /// See [`Destination::name`].
    pub name: String,
    /// See [`Destination::url`].
    pub url: String,
    /// See [`Destination::template`].
    pub template: Template,
    /// See [`Destination::events`].
    pub events: Vec<HookEvent>,
    /// See [`Destination::include_body`].
    pub include_body: bool,
    /// See [`Destination::enabled`].
    pub enabled: bool,
    /// See [`Destination::secret`].
    pub secret: CredentialSource,
    /// See [`Destination::max_attempts`].
    pub max_attempts: i64,
}

impl Default for NewDestination {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            template: Template::Generic,
            events: Vec::new(),
            // Both of the fields that decide how much leaves the machine
            // default to the quiet answer. A caller has to say so.
            include_body: false,
            enabled: true,
            secret: CredentialSource::None,
            max_attempts: 5,
        }
    }
}

/// One row of the persisted delivery queue, as an operator sees it.
///
/// `Debug` is hand-written rather than derived: this type holds `payload`,
/// which is the redacted-but-still-private mail content that left the machine.
/// A derived `Debug` would put all of it into any future
/// `tracing::debug!(?delivery, ...)` — a log stream with different retention
/// and different access than the database the same bytes are stored in. Same
/// discipline as [`crate::credential::Secret`] and
/// [`crate::ai::redact::GuardedRequest`].
#[derive(Clone, PartialEq, Eq)]
pub struct Delivery {
    /// Row id — also the `X-Rmail-Delivery` header value.
    pub id: i64,
    /// The destination this is for.
    pub destination_id: i64,
    /// The idempotency key.
    pub event_key: String,
    /// The event wire string, or [`FORWARD_EVENT`].
    pub event: String,
    /// The message it is about, when the local copy still exists.
    pub message_id: Option<i64>,
    /// The exact bytes POSTed — see V48 on why they are frozen.
    pub payload: String,
    /// Where it has got to.
    pub state: DeliveryState,
    /// Attempts made.
    pub attempts: i64,
    /// This delivery's cap.
    pub max_attempts: i64,
    /// Unix seconds before which it must not be attempted.
    pub next_attempt_at: Option<i64>,
    /// The last HTTP status, when the peer answered at all.
    pub last_status: Option<i64>,
    /// The last failure, as a short operator-facing string. Never a response
    /// body verbatim: a remote server's error page is text this daemon did not
    /// write, and storing it wholesale puts an attacker-influenced blob into
    /// every `mail webhook deliveries` listing.
    pub last_error: Option<String>,
    /// When it was enqueued.
    pub created_at: i64,
    /// When it was accepted, if it was.
    pub delivered_at: Option<i64>,
}

impl std::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delivery")
            .field("id", &self.id)
            .field("destination_id", &self.destination_id)
            .field("event", &self.event)
            .field("event_key", &self.event_key)
            .field("message_id", &self.message_id)
            .field("state", &self.state)
            .field("attempts", &self.attempts)
            .field("max_attempts", &self.max_attempts)
            .field("next_attempt_at", &self.next_attempt_at)
            .field("last_status", &self.last_status)
            .field("last_error", &self.last_error)
            // Never the bytes — see the type's own docs.
            .field("payload", &format_args!("<{} bytes>", self.payload.len()))
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Event subscriptions
// ---------------------------------------------------------------------------

/// Serialize a subscription for `webhook_destinations.events`.
#[must_use]
pub fn join_events(events: &[HookEvent]) -> String {
    events
        .iter()
        .map(|e| hook_event_str(*e))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a stored subscription. An unrecognized entry is dropped with a
/// warning rather than making the whole destination unreadable — the same
/// forward-compatibility call `ai::redact::enabled_kinds` makes for an unknown
/// pattern name.
#[must_use]
pub fn split_events(events: &str) -> Vec<HookEvent> {
    events
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let parsed = parse_hook_event(s);
            if parsed.is_none() {
                tracing::warn!(event = s, "unknown webhook event subscription; ignoring it");
            }
            parsed
        })
        .collect()
}

/// The wire/TOML string for a [`HookEvent`] — the same vocabulary
/// `[[hooks.hooks]] event = "..."` uses, deliberately shared rather than
/// re-invented. A webhook and a hook subscribe to the same set of things
/// happening; giving them two spellings would mean an operator has to learn
/// which surface they are on.
#[must_use]
pub const fn hook_event_str(event: HookEvent) -> &'static str {
    match event {
        HookEvent::OnNewMessage => "on_new_message",
        HookEvent::OnLabel => "on_label",
        HookEvent::OnMove => "on_move",
        HookEvent::OnRuleMatch => "on_rule_match",
        HookEvent::OnSyncError => "on_sync_error",
    }
}

/// Parse [`hook_event_str`]'s output.
#[must_use]
pub fn parse_hook_event(value: &str) -> Option<HookEvent> {
    match value {
        "on_new_message" => Some(HookEvent::OnNewMessage),
        "on_label" => Some(HookEvent::OnLabel),
        "on_move" => Some(HookEvent::OnMove),
        "on_rule_match" => Some(HookEvent::OnRuleMatch),
        "on_sync_error" => Some(HookEvent::OnSyncError),
        _ => None,
    }
}

/// The idempotency key for a dispatched event: the durable log's own `seq`,
/// which is unique and monotonic for the life of the database.
#[must_use]
pub fn event_key_for_seq(seq: i64) -> String {
    format!("event:{seq}")
}

/// The idempotency key for a manual forward.
///
/// Includes `at` (unix seconds) because forwarding the same message to the
/// same channel twice is a legitimate act a human just performed — unlike an
/// event, which is one fact that happened once. Two forwards inside the same
/// second do collapse to one, which is the right way round: that is a
/// double-click, not two decisions.
#[must_use]
pub fn event_key_for_forward(message_id: i64, at: i64) -> String {
    format!("forward:{message_id}:{at}")
}

// ---------------------------------------------------------------------------
// URL policy
// ---------------------------------------------------------------------------

/// Whether this daemon will POST mail content to `url`.
///
/// Requires `https`, except for a loopback host where plaintext is allowed
/// because the request cannot leave the machine (and because that is what the
/// tests drive). Also rejects a URL carrying userinfo (`https://user:pw@host/`),
/// which is a credential this daemon would be transmitting on the operator's
/// behalf without any of the handling a [`Secret`] gets.
///
/// Called at registration *and* immediately before every POST — see the module
/// docs on why a stored row is re-checked.
///
/// # Errors
/// [`Error::InvalidArgument`] with a message naming the specific rule.
pub fn validate_url(url: &str) -> Result<(), Error> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| Error::invalid_argument(format!("not a valid webhook URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::invalid_argument("a webhook URL needs a host".to_owned()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::invalid_argument(
            "a webhook URL must not carry userinfo; put the credential in the signing key \
             (--secret-env/--secret-command/--secret-keychain) instead"
                .to_owned(),
        ));
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(host) => Ok(()),
        "http" => Err(Error::invalid_argument(format!(
            "a webhook URL must be https (plaintext http is allowed only for loopback, not \
             {host:?}) — mail content and a signature would otherwise travel in the clear"
        ))),
        other => Err(Error::invalid_argument(format!(
            "unsupported webhook URL scheme {other:?}: only https (and http on loopback) are \
             delivered to"
        ))),
    }
}

/// Whether `host` is unambiguously this machine.
///
/// `localhost` by name, or any address `IpAddr::is_loopback` accepts —
/// including the whole `127.0.0.0/8` block and `::1`. Deliberately *not* a
/// substring or suffix test: `localhost.attacker.example` resolves wherever an
/// attacker's DNS says, and `notlocalhost` is not this machine either.
fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `Url::host_str` keeps the brackets off an IPv6 literal, but a
    // hand-written host may still carry them.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// A destination URL reduced to what is safe to put in a log line: scheme,
/// host and port, never the path or query.
///
/// A Slack incoming-webhook URL *is* the credential — anyone holding
/// `https://hooks.slack.com/services/T…/B…/…` can post to that channel — and
/// so are the opaque paths most SaaS receivers hand out. Logging the whole URL
/// on every failed delivery would copy that credential into a log stream with
/// different retention and different access than the database it is stored in.
#[must_use]
pub fn log_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", parsed.scheme()),
            (Some(host), None) => format!("{}://{host}", parsed.scheme()),
            (None, _) => parsed.scheme().to_owned(),
        },
        Err(_) => "<unparseable>".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// What one POST attempt decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    /// The endpoint answered 2xx.
    Delivered {
        /// The status it answered with.
        status: u16,
    },
    /// Worth trying again: a 5xx, a 408/429, or a transport failure.
    Retry {
        /// The status, when the peer answered at all.
        status: Option<u16>,
        /// A short operator-facing reason. Never a response body.
        reason: String,
    },
    /// Not worth trying again: a 3xx (this client does not follow redirects,
    /// and a redirecting destination is misconfigured), any other 4xx, or a
    /// URL/credential this daemon refuses to use.
    Permanent {
        /// The status, when the peer answered at all.
        status: Option<u16>,
        /// A short operator-facing reason.
        reason: String,
    },
    /// The daemon is shutting down; this attempt was abandoned mid-flight.
    ///
    /// Its own variant rather than a [`Self::Retry`] because the *attempt* is
    /// refunded. A shutdown is not the endpoint's fault, and charging one is
    /// how a daemon restarted a few times during a backoff window silently
    /// spends a delivery's whole cap and marks it `failed` without any
    /// endpoint ever having refused it. The cost of refunding is that a
    /// request which did reach the peer before the cancellation may be sent
    /// again — which this queue is already at-least-once about, and which the
    /// stable `X-Rmail-Delivery` id exists for the receiver to dedupe on.
    Cancelled,
}

/// Build the HTTP client every delivery goes through.
///
/// # Errors
/// [`Error::Unavailable`] if the TLS/transport stack cannot be constructed.
pub fn build_client(timeout: Duration) -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .timeout(timeout)
        // Never follow a redirect on a POST carrying mail content — see the
        // module docs.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::unavailable(format!("could not build the webhook client: {e}")))
}

/// POST one delivery, once.
///
/// Never returns `Err`: a failure to build a request, resolve a key or reach
/// the endpoint is an [`Attempt`] the caller records, not a reason to abort a
/// tick. That is [`crate::hooks::run_hook`]'s contract and it exists for the
/// same reason — one broken destination must not stall the queue behind it.
#[tracing::instrument(
    skip(client, delivery, key, cancel),
    fields(
        delivery_id = delivery.id,
        destination = %delivery.destination.name,
        url = %log_url(&delivery.destination.url),
        outcome
    )
)]
pub async fn send(
    client: &reqwest::Client,
    delivery: &ClaimedDelivery,
    key: Option<&Secret>,
    now: i64,
    cancel: &CancellationToken,
) -> Attempt {
    let span = tracing::Span::current();
    let destination = &delivery.destination;
    let body = delivery.payload.as_str();

    // Re-checked here, not only at registration — see the module docs.
    if let Err(error) = validate_url(&destination.url) {
        let attempt = Attempt::Permanent {
            status: None,
            reason: error.to_string(),
        };
        span.record("outcome", "permanent");
        return attempt;
    }

    let mut request = client
        .post(&destination.url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(sign::DELIVERY_HEADER, delivery.id.to_string())
        .header(sign::EVENT_HEADER, delivery.event.as_str())
        .header(sign::TIMESTAMP_HEADER, now.to_string())
        .body(body.to_owned());
    if let Some(key) = key {
        request = request.header(
            sign::SIGNATURE_HEADER,
            sign::sign(key, now, body.as_bytes()),
        );
    }

    let response = tokio::select! {
        () = cancel.cancelled() => {
            span.record("outcome", "cancelled");
            return Attempt::Cancelled;
        }
        response = request.send() => response,
    };

    let attempt = match response {
        Ok(response) => classify(response.status()),
        Err(error) => Attempt::Retry {
            status: None,
            // `reqwest::Error`'s Display can include the URL it was built
            // from, which is exactly what `log_url` exists to keep out of
            // logs and out of `webhook_deliveries.last_error`. So this
            // reports the *class* of transport failure and the redacted
            // authority, never the error's own text.
            reason: transport_reason(&error, &destination.url),
        },
    };
    span.record(
        "outcome",
        match &attempt {
            Attempt::Delivered { .. } => "delivered",
            Attempt::Retry { .. } => "retry",
            Attempt::Permanent { .. } => "permanent",
            Attempt::Cancelled => "cancelled",
        },
    );
    attempt
}

/// Map an HTTP status onto a retry decision.
fn classify(status: reqwest::StatusCode) -> Attempt {
    let code = status.as_u16();
    if status.is_success() {
        return Attempt::Delivered { status: code };
    }
    if status.is_redirection() {
        return Attempt::Permanent {
            status: Some(code),
            reason: format!(
                "the endpoint answered {code} (a redirect). Redirects are never followed on a \
                 request carrying mail content; register the final URL instead"
            ),
        };
    }
    // 408 Request Timeout and 429 Too Many Requests are the two 4xx codes that
    // say "later", not "no". Everything else in the 4xx range is a statement
    // about the request itself and will be answered identically forever.
    if status.is_server_error() || code == 408 || code == 429 {
        return Attempt::Retry {
            status: Some(code),
            reason: format!("the endpoint answered {code}"),
        };
    }
    Attempt::Permanent {
        status: Some(code),
        reason: format!("the endpoint answered {code}"),
    }
}

/// Describe a transport failure without echoing `reqwest`'s own message (which
/// embeds the URL) — see [`send`].
fn transport_reason(error: &reqwest::Error, url: &str) -> String {
    let what = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not be connected to"
    } else if error.is_request() {
        "could not be requested"
    } else if error.is_body() || error.is_decode() {
        "answered a body that could not be read"
    } else {
        "could not be reached"
    };
    format!("{} {what}", log_url(url))
}

/// Resolve a destination's signing key, off the async runtime.
///
/// [`CredentialSource::resolve`] runs a command / hits the keychain
/// synchronously, so it goes through `spawn_blocking` exactly as every other
/// credential call site in this workspace does.
///
/// # The destination's name is the keychain *account*
///
/// `destination` is passed as [`CredentialSource::resolve`]'s `username`,
/// which is the field a macOS generic-password item is addressed by alongside
/// its service. Passing `None` — the obvious-looking thing, since a webhook
/// has no user — makes [`CredentialSource::Keychain`] fail unconditionally
/// (`resolve_keychain` requires it), which would not be a visible bug: the
/// failure surfaces as a deferred delivery, then a `failed` one after the
/// attempt cap, on a destination the operator configured exactly as the CLI
/// documents. So a keychain-backed destination's item is
/// `(service = --secret-keychain, account = the destination's name)`, and
/// `mail webhook add` documents that pairing.
///
/// # Errors
/// [`Error::Unauthenticated`] (or whatever the source reports) when a
/// configured key cannot be obtained. `Ok(None)` for
/// [`CredentialSource::None`] — an unsigned destination, not a failure.
pub async fn resolve_key(
    source: &CredentialSource,
    destination: &str,
) -> Result<Option<Secret>, Error> {
    if matches!(source, CredentialSource::None) {
        return Ok(None);
    }
    let source = source.clone();
    let account = destination.to_owned();
    match tokio::task::spawn_blocking(move || source.resolve(Some(&account))).await {
        Ok(result) => result,
        Err(join_error) => Err(Error::internal(format!(
            "resolving a webhook signing key panicked: {join_error}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Enqueue: the two ways something enters the queue
// ---------------------------------------------------------------------------

/// Build a payload for `message_id` and queue it for `destination`.
///
/// Returns the new delivery's id, or `None` when this `(destination,
/// event_key)` was already queued.
///
/// # Errors
/// [`Error::NotFound`] if the message does not exist, or a mapped storage
/// error.
pub async fn enqueue_for_message(
    db: &Database,
    destination: &Destination,
    event: &str,
    event_key: &str,
    message_id: i64,
    privacy: &AiPrivacy,
) -> Result<Option<i64>, Error> {
    // Read out of the database first, then hand a *pure* renderer to
    // `store::enqueue`, which runs it inside the insert's own transaction so
    // the row is never visible — and therefore never claimable — carrying
    // anything but its final body. See that function's own docs.
    let facts = store::facts_for(db, message_id, destination.include_body).await?;
    let template = destination.template;
    let include_body = destination.include_body;
    let privacy = privacy.clone();
    let event_owned = event.to_owned();
    store::enqueue(
        db,
        destination.id,
        event_key,
        event,
        Some(message_id),
        destination.max_attempts,
        move |id| {
            payload::build(template, &event_owned, id, &facts, include_body, &privacy).to_string()
        },
    )
    .await
}

/// The manual forward action: push one message to one named destination now
/// (`mail forward <id> --to <name>`).
///
/// Queues rather than sending inline, deliberately. The retry, the backoff,
/// the attempt cap and the durable record are properties of the queue, and a
/// forward that bypassed it would be the one delivery in the system with none
/// of them — silently lost if the endpoint happened to be restarting.
///
/// # Errors
/// [`Error::NotFound`] for an unknown destination or message,
/// [`Error::FailedPrecondition`] for a disabled destination, or a mapped
/// storage error.
pub async fn forward(
    db: &Database,
    destination_name: &str,
    message_id: i64,
    privacy: &AiPrivacy,
    now: i64,
) -> Result<i64, Error> {
    let destination = store::get_by_name(db, destination_name).await?;
    if !destination.enabled {
        return Err(Error::failed_precondition(format!(
            "the webhook destination {destination_name:?} is disabled"
        )));
    }
    let key = event_key_for_forward(message_id, now);
    let queued =
        enqueue_for_message(db, &destination, FORWARD_EVENT, &key, message_id, privacy).await?;
    match queued {
        Some(id) => Ok(id),
        // The `(destination, event_key)` fence caught a double-click. The
        // caller asked for this message to reach this destination and it is
        // already queued to, so this is a success naming the existing delivery
        // rather than an error. Looked up by the key itself, not by scanning a
        // page of recent deliveries — a busy destination could push the
        // duplicate off any bounded page, and this must not depend on how much
        // else has happened since.
        None => store::get_by_event_key(db, destination.id, &key)
            .await?
            .map(|delivery| delivery.id)
            .ok_or_else(|| {
                Error::internal(
                    "a forward was refused as a duplicate but no matching delivery exists"
                        .to_owned(),
                )
            }),
    }
}

// ---------------------------------------------------------------------------
// The dispatcher
// ---------------------------------------------------------------------------

/// What one [`WebhookDispatcher::tick`] did — for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebhookTickReport {
    /// Deliveries queued from newly-logged events this tick.
    pub queued: u64,
    /// Deliveries attempted this tick.
    pub attempted: u64,
    /// Attempts the endpoint accepted.
    pub delivered: u64,
    /// Attempts that will be retried.
    pub deferred: u64,
    /// Deliveries that reached the terminal `failed` state this tick.
    pub failed: u64,
}

/// The daemon-side consumer: turns durable events into queued deliveries, and
/// drains the queue.
#[derive(Clone)]
pub struct WebhookDispatcher {
    db: Database,
    events: EventLog,
    client: reqwest::Client,
    privacy: AiPrivacy,
    semaphore: Arc<Semaphore>,
    tick_interval: Duration,
    delivery_timeout: Duration,
    backoff_base: Duration,
    backoff_max: Duration,
    max_batch: i64,
    /// Negative until seeded from [`EventLog::latest_seq`] — see the module
    /// docs' "The cursor starts at 'now'".
    cursor: Arc<AtomicI64>,
}

impl std::fmt::Debug for WebhookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookDispatcher")
            .field("tick_interval", &self.tick_interval)
            .field("delivery_timeout", &self.delivery_timeout)
            .field("max_batch", &self.max_batch)
            .finish_non_exhaustive()
    }
}

impl WebhookDispatcher {
    /// Cursor value meaning "not yet seeded" — never a real `seq`.
    const UNSEEDED_CURSOR: i64 = -1;

    /// Build a dispatcher over `db`/`events`.
    ///
    /// # Errors
    /// [`Error::Unavailable`] if the HTTP client cannot be built.
    pub fn new(
        db: Database,
        events: EventLog,
        config: &WebhooksConfig,
        privacy: AiPrivacy,
    ) -> Result<Self, Error> {
        let delivery_timeout = config.delivery_timeout.as_duration();
        Ok(Self {
            db,
            events,
            client: build_client(delivery_timeout)?,
            privacy,
            semaphore: Arc::new(Semaphore::new(config.max_concurrency.max(1) as usize)),
            tick_interval: config.tick_interval.as_duration().max(MIN_TICK_INTERVAL),
            delivery_timeout,
            backoff_base: config.backoff_base.as_duration(),
            backoff_max: config.backoff_max.as_duration(),
            max_batch: i64::from(config.max_batch.max(1)),
            cursor: Arc::new(AtomicI64::new(Self::UNSEEDED_CURSOR)),
        })
    }

    /// Override the tick interval (tests, and an operator who wants a fast
    /// loop).
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval.max(MIN_TICK_INTERVAL);
        self
    }

    /// One cycle: queue what the event log has newly said, then drain what the
    /// queue owes.
    ///
    /// # Errors
    /// A mapped storage error. Never a single destination's own failure — see
    /// [`send`]'s contract.
    #[tracing::instrument(skip(self, cancel), fields(queued, attempted, delivered, failed))]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<WebhookTickReport, Error> {
        let queued = self.queue_new_events().await?;
        let drained = self.deliver_due(cancel).await?;
        let report = WebhookTickReport {
            queued,
            attempted: drained.attempted,
            delivered: drained.delivered,
            deferred: drained.deferred,
            failed: drained.failed,
        };

        let span = tracing::Span::current();
        span.record("queued", report.queued);
        span.record("attempted", report.attempted);
        span.record("delivered", report.delivered);
        span.record("failed", report.failed);
        Ok(report)
    }

    /// Drain newly-logged events and queue a delivery per subscribed,
    /// enabled destination.
    ///
    /// Reads the destination list once per tick rather than caching it, so
    /// `Register`/`Remove` take effect on the next tick without a restart —
    /// the difference from [`crate::hooks::HookDispatcher`], whose hooks come
    /// from a config file read at boot.
    async fn queue_new_events(&self) -> Result<u64, Error> {
        let destinations: Vec<Destination> = store::list(&self.db)
            .await?
            .into_iter()
            .filter(|d| d.enabled && !d.events.is_empty())
            .collect();

        let mut since = self.cursor.load(Ordering::SeqCst);
        if since == Self::UNSEEDED_CURSOR {
            since = self.events.latest_seq().await?.unwrap_or(0);
        }
        // Even with no subscribed destination the cursor must still advance,
        // or a destination registered later would be handed every event since
        // the daemon started — the flood the module docs' "cursor starts at
        // now" exists to prevent, arriving by a different route.
        let mut cursor = since;
        let mut queued = 0u64;
        let mut recovered_once = false;
        'drain: loop {
            let page = match self.events.since(cursor, DRAIN_PAGE).await {
                Ok(page) => page,
                Err(error) if error.reason() == ErrorReason::OutOfRange && !recovered_once => {
                    let head = self.events.latest_seq().await?.unwrap_or(0);
                    tracing::warn!(
                        cursor,
                        head,
                        %error,
                        "webhook dispatch cursor fell behind the event log's retention window; \
                         resuming from the current head rather than replaying history"
                    );
                    cursor = head;
                    recovered_once = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let got = page.events.len();
            for event in &page.events {
                cursor = event.seq;
                let Some(message_id) = event.message_id else {
                    // Every payload this module builds is about a message. An
                    // event with no message (a sync-state change, say) has
                    // nothing to say in that shape, so it is skipped rather
                    // than delivered as a husk.
                    continue;
                };
                for destination in &destinations {
                    if !destination
                        .events
                        .iter()
                        .any(|subscribed| crate::hooks::hook_matches(*subscribed, event))
                    {
                        continue;
                    }
                    let subscribed = destination
                        .events
                        .iter()
                        .copied()
                        .find(|e| crate::hooks::hook_matches(*e, event))
                        .map_or(FORWARD_EVENT, hook_event_str);
                    match enqueue_for_message(
                        &self.db,
                        destination,
                        subscribed,
                        &event_key_for_seq(event.seq),
                        message_id,
                        &self.privacy,
                    )
                    .await
                    {
                        Ok(Some(_)) => queued += 1,
                        // Already queued by an overlapping tick — the fence
                        // doing its job, not a problem.
                        Ok(None) => {}
                        // A message that has since been expunged, or a
                        // storage hiccup on one row. One destination's
                        // enqueue failing must not stall the cursor behind
                        // it — the same call `hooks::tick` makes for a hook
                        // that could not be spawned.
                        Err(error) => tracing::warn!(
                            destination = %destination.name,
                            message_id,
                            %error,
                            "could not queue a webhook delivery for this event"
                        ),
                    }
                }
                if queued >= u64::try_from(self.max_batch).unwrap_or(u64::MAX) {
                    break 'drain;
                }
            }
            cursor = page.next_seq;
            if i64::try_from(got).unwrap_or(i64::MAX) < DRAIN_PAGE {
                break;
            }
        }
        self.cursor.store(cursor, Ordering::SeqCst);
        Ok(queued)
    }

    /// Claim and attempt every due delivery, bounded by the shared semaphore.
    async fn deliver_due(&self, cancel: &CancellationToken) -> Result<Drained, Error> {
        // The claim clock only. Each attempt takes its *own* timestamp when it
        // builds its request — see `attempt` on why a batch-wide one is a bug
        // rather than an optimization.
        let now = chrono::Utc::now().timestamp();
        // The lease covers one attempt plus its timeout, so a process that
        // dies mid-attempt leaves a row that becomes claimable again rather
        // than one that is stuck — `notify::repo::claim_due`'s own reasoning.
        let lease = i64::try_from(self.delivery_timeout.as_secs()).unwrap_or(i64::MAX / 4) + 30;
        let claimed = store::claim_due(&self.db, now, lease, self.max_batch).await?;
        if claimed.is_empty() {
            return Ok(Drained::default());
        }

        let mut set = tokio::task::JoinSet::new();
        for delivery in claimed {
            let this = self.clone();
            let cancel = cancel.clone();
            let span = tracing::info_span!(
                "webhook_delivery",
                delivery_id = delivery.id,
                destination = %delivery.destination.name,
            );
            set.spawn(
                async move {
                    let Ok(_permit) = this.semaphore.clone().acquire_owned().await else {
                        // The semaphore is never closed by this dispatcher, so
                        // this is unreachable — but the attempt `claim_due`
                        // already charged has to go back either way, or an
                        // unreachable branch would quietly be the one path
                        // that eats a delivery's cap.
                        return this.give_back(&delivery).await;
                    };
                    this.attempt(delivery, &cancel).await
                }
                .instrument(span),
            );
        }

        let mut drained = Drained::default();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Outcome::Cancelled) => {}
                Ok(Outcome::Delivered) => {
                    drained.attempted += 1;
                    drained.delivered += 1;
                }
                Ok(Outcome::Deferred) => {
                    drained.attempted += 1;
                    drained.deferred += 1;
                }
                Ok(Outcome::Failed) => {
                    drained.attempted += 1;
                    drained.failed += 1;
                }
                Err(join_error) => {
                    tracing::error!(error = %join_error, "a webhook delivery task panicked");
                }
            }
        }
        Ok(drained)
    }

    /// One delivery, end to end: resolve the key, POST, record.
    async fn attempt(&self, delivery: ClaimedDelivery, cancel: &CancellationToken) -> Outcome {
        // Checked before anything expensive, not only inside `send`'s own
        // `select!`. `resolve_key` goes through `spawn_blocking`, which cannot
        // be aborted and can sit for `credential`'s command timeout on a
        // wedged `password_command` — so without this a shutdown would wait
        // out one such command per claimed row before any of them noticed
        // they had been cancelled.
        if cancel.is_cancelled() {
            return self.give_back(&delivery).await;
        }
        // Re-checked at the moment of the attempt rather than trusting the
        // claim: a destination disabled between the two must not still be
        // POSTed to. The same reasoning as re-running `validate_url` in
        // `send` — a stored row is not a licence.
        if !delivery.destination.enabled {
            return self.give_back(&delivery).await;
        }
        let key = match resolve_key(&delivery.destination.secret, &delivery.destination.name).await
        {
            Ok(key) => key,
            Err(error) => {
                // A configured key that cannot be resolved is not a transport
                // problem and will not fix itself by being retried sooner; it
                // still gets the ordinary backoff (an operator restoring a
                // keychain entry should not have to replay by hand), and the
                // error is recorded without the reference it names.
                let reason = format!("the signing key could not be resolved: {error}");
                return self.record_retry(&delivery, None, &reason).await;
            }
        };
        // The signature's timestamp is taken *here*, immediately before the
        // request is built — never the batch's own claim time.
        //
        // A receiver is expected to reject a signature outside a freshness
        // window (five minutes is what Slack and Stripe both document, and
        // `sign`'s module docs tell receivers to enforce one). A whole batch
        // sharing one timestamp meant the last request of a slow batch could
        // be minutes behind its own clock — `max_batch` deliveries at
        // `max_concurrency` parallelism, each up to `delivery_timeout`, plus
        // whatever `resolve_key`'s command costs — and be refused as stale.
        // `classify` reads that refusal as a 4xx, which is *terminal*, so the
        // delivery would die on its first attempt for a reason that had
        // nothing to do with it.
        let now = chrono::Utc::now().timestamp();
        let attempt = send(&self.client, &delivery, key.as_ref(), now, cancel).await;
        match attempt {
            Attempt::Delivered { status } => {
                if let Err(error) = store::mark_delivered(&self.db, delivery.id, status).await {
                    tracing::warn!(%error, "could not record a delivered webhook");
                }
                Outcome::Delivered
            }
            Attempt::Retry { status, reason } => {
                self.record_retry(&delivery, status, &reason).await
            }
            Attempt::Permanent { status, reason } => {
                // Terminal on the first refusal: retrying a 404 or a 401 four
                // more times is four more requests to somebody else's server
                // with the same answer waiting.
                if let Err(error) = store::mark_failed(&self.db, delivery.id, status, &reason).await
                {
                    tracing::warn!(%error, "could not record a failed webhook");
                }
                tracing::warn!(
                    destination = %delivery.destination.name,
                    url = %log_url(&delivery.destination.url),
                    status = ?status,
                    reason = %reason,
                    "webhook delivery permanently refused"
                );
                Outcome::Failed
            }
            Attempt::Cancelled => self.give_back(&delivery).await,
        }
    }

    /// Return a claimed delivery to the queue with its attempt refunded — the
    /// shared tail of every path that abandons an attempt without the endpoint
    /// having answered. See [`Attempt::Cancelled`] on why a shutdown must not
    /// spend a delivery's cap.
    async fn give_back(&self, delivery: &ClaimedDelivery) -> Outcome {
        let now = chrono::Utc::now().timestamp();
        if let Err(error) = store::refund(&self.db, delivery.id, now).await {
            tracing::warn!(%error, "could not return an abandoned webhook to the queue");
        }
        Outcome::Cancelled
    }

    /// Back off, or give up if this attempt was the last one allowed.
    async fn record_retry(
        &self,
        delivery: &ClaimedDelivery,
        status: Option<u16>,
        reason: &str,
    ) -> Outcome {
        if delivery.attempts >= delivery.max_attempts {
            if let Err(error) = store::mark_failed(
                &self.db,
                delivery.id,
                status,
                &format!("{reason}; attempts exhausted ({})", delivery.max_attempts),
            )
            .await
            {
                tracing::warn!(%error, "could not record an exhausted webhook");
            }
            tracing::warn!(
                destination = %delivery.destination.name,
                url = %log_url(&delivery.destination.url),
                attempts = delivery.attempts,
                reason = %reason,
                "webhook delivery gave up after its attempt cap"
            );
            return Outcome::Failed;
        }
        let at = chrono::Utc::now().timestamp()
            + i64::try_from(self.backoff(delivery.attempts).as_secs()).unwrap_or(i64::MAX / 4);
        if let Err(error) = store::defer(&self.db, delivery.id, at, status, reason).await {
            tracing::warn!(%error, "could not defer a webhook delivery");
        }
        tracing::debug!(
            destination = %delivery.destination.name,
            attempts = delivery.attempts,
            reason = %reason,
            "webhook delivery deferred"
        );
        Outcome::Deferred
    }

    /// Exponential backoff: `base * 2^(attempts - 1)`, clamped to
    /// `backoff_max`. Computed by doubling rather than by `pow` so a large
    /// attempt count saturates instead of overflowing.
    fn backoff(&self, attempts: i64) -> Duration {
        let mut delay = self.backoff_base;
        for _ in 1..attempts.max(1) {
            delay = delay.saturating_mul(2);
            if delay >= self.backoff_max {
                return self.backoff_max;
            }
        }
        delay.min(self.backoff_max)
    }

    /// Seed the cursor at the event log's current head, before `spawn`
    /// returns — the identical guarantee, and the identical boot-window bug it
    /// avoids, as [`crate::hooks::HookDispatcher::seed_cursor`].
    async fn seed_cursor(&self) {
        if self.cursor.load(Ordering::SeqCst) != Self::UNSEEDED_CURSOR {
            return;
        }
        match self.events.latest_seq().await {
            Ok(head) => self.cursor.store(head.unwrap_or(0), Ordering::SeqCst),
            Err(error) => tracing::warn!(
                %error,
                "could not seed the webhook dispatch cursor at startup; it will be seeded on \
                 the first tick instead, which may skip events appended in between"
            ),
        }
    }

    /// Spawn the periodic loop, ticking once immediately and then on the
    /// configured interval until `cancel` fires.
    pub async fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        self.seed_cursor().await;
        tokio::spawn(async move {
            loop {
                match self.tick(&cancel).await {
                    Ok(report) => tracing::debug!(?report, "webhook dispatch tick"),
                    Err(error) => tracing::warn!(%error, "webhook dispatch tick failed"),
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
            }
        })
    }
}

/// One delivery task's result.
enum Outcome {
    Delivered,
    Deferred,
    Failed,
    /// Abandoned mid-flight by a shutdown, and returned to the queue with its
    /// attempt refunded — see [`Attempt::Cancelled`]. Not counted as an
    /// attempt in [`WebhookTickReport`], because the row's own `attempts` is
    /// not counting it either.
    Cancelled,
}

/// What [`WebhookDispatcher::deliver_due`] did.
#[derive(Debug, Default, Clone, Copy)]
struct Drained {
    attempted: u64,
    delivered: u64,
    deferred: u64,
    failed: u64,
}

#[cfg(test)]
mod tests;
