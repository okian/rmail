//! `mail webhook add|list|rm|deliveries|replay` and `mail forward` — the
//! operator surface for outbound webhooks (task 68, prd.md #49 and #64).
//!
//! # Why `add` is an RPC here, when `mail hook add` edits a file
//!
//! `hook_cli::add` writes a `[[hooks.hooks]]` block into the operator's TOML
//! because a hook *is* configuration: a command line, read once at boot. A
//! webhook destination is not. It owns a live delivery queue, an attempt
//! history and a terminal failure somebody has to replay out of, all keyed by
//! its row id — none of which a TOML file can hold, and all of which would
//! immediately drift from a file that claimed to be the source of truth. So
//! every verb here is a real call to the running daemon, and the destination
//! takes effect on the dispatcher's next tick rather than at the next restart.
//!
//! # `--to slack:eng-alerts`
//!
//! prd.md #64 writes the forward target as `slack:<name>`. The prefix is
//! sugar: it names the *kind* of thing being addressed for a human reading the
//! command back, and this module strips it before the RPC. What actually
//! decides the rendering is the destination's own registered template, because
//! a caller who could pick the template could pick a shape the operator's
//! receiver does not accept and blame the daemon for the 400. A bare name
//! works identically.
//!
//! # The signing key is never typed on this command line
//!
//! `--secret-env`/`--secret-command`/`--secret-keychain` name *where the key
//! lives*; there is deliberately no `--secret <value>`. A key passed as an
//! argument is a key in the shell history, in `ps` output for the lifetime of
//! the process, and in whatever collects either. This is the same rule
//! `[[accounts]]` has enforced since V3 and the reason `webhook_destinations`
//! stores a reference rather than a secret.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use rmail_proto::v1::webhook_service_client::WebhookServiceClient;
use rmail_proto::v1::{
    ForwardMessageRequest, ListDeliveriesRequest, ListWebhooksRequest, RegisterWebhookRequest,
    RemoveWebhookRequest, ReplayDeliveryRequest, SetWebhookEnabledRequest, WebhookDelivery,
    WebhookDeliveryState, WebhookDestination, WebhookEvent, WebhookSecretSource, WebhookTemplate,
};

/// `mail webhook <action>`.
#[derive(Debug, Subcommand)]
pub enum WebhookAction {
    /// Register an outbound destination (`WebhookService.Register`).
    Add(AddArgs),
    /// List registered destinations (`WebhookService.List`).
    List(ListArgs),
    /// Remove a destination and its delivery history
    /// (`WebhookService.Remove`).
    Rm(RmArgs),
    /// Resume sending to a destination (`WebhookService.SetEnabled`).
    Enable(RmArgs),
    /// Stop sending to a destination without forgetting where it pointed
    /// (`WebhookService.SetEnabled`).
    ///
    /// Queued deliveries stay queued and stop being sent, so enabling again
    /// resumes rather than replays. Use `rm` to forget the destination
    /// entirely.
    Disable(RmArgs),
    /// Inspect the delivery queue (`WebhookService.ListDeliveries`).
    Deliveries(DeliveriesArgs),
    /// Re-arm one delivery for another attempt
    /// (`WebhookService.ReplayDelivery`).
    Replay(ReplayArgs),
}

/// `mail webhook add <url> --name <name> --events ...`.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// Where deliveries are POSTed. Must be https, except on loopback.
    url: String,
    /// The handle `mail forward --to` and `mail webhook rm` address it by.
    #[arg(long)]
    name: String,
    /// Payload rendering.
    #[arg(long, value_enum, default_value_t = TemplateArg::Generic)]
    template: TemplateArg,
    /// Events to subscribe to. Omit for a destination that only ever receives
    /// an explicit `mail forward`.
    #[arg(long, value_enum, num_args = 1.., value_delimiter = ',')]
    events: Vec<EventArg>,
    /// Let this destination receive message *bodies*.
    ///
    /// Off unless given. The default payload is sender, subject, a deep link
    /// and (when the AI passes have run) a two-sentence summary — enough to
    /// decide whether to go and read the message. Turning this on ships the
    /// body text itself, redacted, to a third party on every matching message.
    #[arg(long)]
    include_body: bool,
    /// Register it disabled: listed, but nothing is sent to it.
    #[arg(long)]
    disabled: bool,
    /// Name of an environment variable holding the HMAC signing key.
    #[arg(long, group = "secret")]
    secret_env: Option<String>,
    /// Shell command whose stdout is the HMAC signing key.
    #[arg(long, group = "secret")]
    secret_command: Option<String>,
    /// macOS Keychain service holding the HMAC signing key.
    ///
    /// The item is addressed by `(service = this value, account = --name)`, so
    /// create it with the destination's own name as the account field.
    #[arg(long, group = "secret")]
    secret_keychain: Option<String>,
    /// Attempt cap per delivery before it is given up on.
    #[arg(long, default_value_t = 5)]
    max_attempts: i64,
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

/// `mail webhook list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show each destination's full URL, not just its scheme://host.
    ///
    /// Off by default because a webhook URL is frequently the credential
    /// itself — anyone holding a Slack incoming-webhook URL can post to that
    /// channel — and `mail webhook list` output gets pasted into tickets and
    /// chat.
    #[arg(long)]
    reveal_url: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

/// `mail webhook rm <name>`.
#[derive(Debug, Args)]
pub struct RmArgs {
    /// The destination's name.
    name: String,
}

/// `mail webhook deliveries`.
#[derive(Debug, Args)]
pub struct DeliveriesArgs {
    /// Restrict to one destination.
    #[arg(long)]
    destination: Option<String>,
    /// How many rows, newest first.
    #[arg(long, default_value_t = 20)]
    limit: i64,
    /// Include the exact JSON body that was POSTed.
    ///
    /// Off by default: a delivery listing is frequently pasted into a ticket
    /// or a chat, and this field is the mail content the rest of the view
    /// deliberately does not restate.
    #[arg(long)]
    show_payload: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

/// `mail webhook replay <delivery-id>`.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The delivery id, as `mail webhook deliveries` reports it.
    delivery_id: i64,
}

/// `mail forward <message-id> --to <destination>`.
#[derive(Debug, Args)]
pub struct ForwardArgs {
    /// The message to forward.
    message_id: i64,
    /// The destination, as `<name>` or `slack:<name>` (the prefix is sugar —
    /// see this module's own docs).
    #[arg(long)]
    to: String,
    /// Print JSON instead of a line of prose.
    #[arg(long)]
    json: bool,
}

/// Payload rendering, as a `clap` value. Its own type rather than a reuse of
/// `rmail_core::webhooks::Template` because `rmail-core` does not and should
/// not depend on `clap`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum TemplateArg {
    /// A JSON document with the facts as named fields.
    Generic,
    /// Slack's incoming-webhook shape.
    Slack,
}

impl TemplateArg {
    const fn proto(self) -> WebhookTemplate {
        match self {
            Self::Generic => WebhookTemplate::Generic,
            Self::Slack => WebhookTemplate::Slack,
        }
    }
}

/// The subscription vocabulary — the same words `mail hook add` takes, for
/// the reason `rmail_core::webhooks::hook_event_str` gives.
// The shared `On*` prefix is the PRD/config vocabulary itself, not an
// accidental naming collision; `hook_cli::EventArg` carries the identical
// allowance for the identical reason.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum EventArg {
    OnNewMessage,
    OnLabel,
    OnMove,
    OnRuleMatch,
    OnSyncError,
}

impl EventArg {
    const fn proto(self) -> WebhookEvent {
        match self {
            Self::OnNewMessage => WebhookEvent::OnNewMessage,
            Self::OnLabel => WebhookEvent::OnLabel,
            Self::OnMove => WebhookEvent::OnMove,
            Self::OnRuleMatch => WebhookEvent::OnRuleMatch,
            Self::OnSyncError => WebhookEvent::OnSyncError,
        }
    }
}

/// Dispatch `mail webhook <action>`.
///
/// # Errors
/// No daemon, a failed RPC, or an unwritable stdout.
pub async fn run(socket: &Path, action: WebhookAction) -> Result<()> {
    match action {
        WebhookAction::Add(args) => add(socket, args).await,
        WebhookAction::List(args) => list(socket, args).await,
        WebhookAction::Rm(args) => rm(socket, args).await,
        WebhookAction::Enable(args) => set_enabled(socket, args, true).await,
        WebhookAction::Disable(args) => set_enabled(socket, args, false).await,
        WebhookAction::Deliveries(args) => deliveries(socket, args).await,
        WebhookAction::Replay(args) => replay(socket, args).await,
    }
}

async fn add(socket: &Path, args: AddArgs) -> Result<()> {
    let (secret_source, secret_reference) =
        match (args.secret_env, args.secret_command, args.secret_keychain) {
            (Some(name), _, _) => (WebhookSecretSource::Env, name),
            (_, Some(command), _) => (WebhookSecretSource::Command, command),
            (_, _, Some(service)) => (WebhookSecretSource::Keychain, service),
            _ => (WebhookSecretSource::Unspecified, String::new()),
        };
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .register(RegisterWebhookRequest {
            name: args.name,
            url: args.url,
            template: args.template.proto() as i32,
            events: args.events.iter().map(|e| e.proto() as i32).collect(),
            include_body: args.include_body,
            disabled: args.disabled,
            secret_source: secret_source as i32,
            secret_reference,
            max_attempts: args.max_attempts,
        })
        .await
        .context("Register RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    let Some(destination) = response.destination else {
        anyhow::bail!("the daemon registered a destination but returned nothing about it");
    };
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&destination_json(&destination))?
        )?;
        return Ok(());
    }
    writeln!(out, "registered {}", destination.name)?;
    write_destination(&mut out, &destination)?;
    if secret_source == WebhookSecretSource::Unspecified {
        writeln!(
            out,
            "\nnote: no signing key configured — deliveries to this destination are unsigned \
             and the receiver cannot tell they came from rmail. Use --secret-env, \
             --secret-command or --secret-keychain."
        )?;
    }
    if destination.include_body {
        writeln!(
            out,
            "note: this destination receives message BODIES (redacted), not just \
             sender/subject/link."
        )?;
    }
    Ok(())
}

async fn list(socket: &Path, args: ListArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .list(ListWebhooksRequest {
            reveal_url: args.reveal_url,
        })
        .await
        .context("List RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "destinations": response
                    .destinations
                    .iter()
                    .map(destination_json)
                    .collect::<Vec<_>>(),
            }))?
        )?;
        return Ok(());
    }
    if response.destinations.is_empty() {
        writeln!(
            out,
            "no webhook destinations registered — nothing is sent anywhere"
        )?;
        return Ok(());
    }
    for destination in &response.destinations {
        write_destination(&mut out, destination)?;
        writeln!(out)?;
    }
    Ok(())
}

async fn rm(socket: &Path, args: RmArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .remove(RemoveWebhookRequest {
            name: args.name.clone(),
        })
        .await
        .context("Remove RPC failed")?
        .into_inner();
    let mut out = std::io::stdout().lock();
    if response.removed {
        writeln!(out, "removed {} (and its delivery history)", args.name)?;
    } else {
        writeln!(out, "no destination named {}", args.name)?;
    }
    Ok(())
}

async fn set_enabled(socket: &Path, args: RmArgs, enabled: bool) -> Result<()> {
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .set_enabled(SetWebhookEnabledRequest {
            name: args.name.clone(),
            enabled,
        })
        .await
        .context("SetEnabled RPC failed")?
        .into_inner();
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{} is now {}",
        args.name,
        if enabled { "enabled" } else { "disabled" }
    )?;
    if let Some(destination) = response.destination {
        write_destination(&mut out, &destination)?;
    }
    Ok(())
}

async fn deliveries(socket: &Path, args: DeliveriesArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .list_deliveries(ListDeliveriesRequest {
            destination: args.destination.unwrap_or_default(),
            limit: args.limit,
            include_payload: args.show_payload,
        })
        .await
        .context("ListDeliveries RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "deliveries": response
                    .deliveries
                    .iter()
                    .map(delivery_json)
                    .collect::<Vec<_>>(),
            }))?
        )?;
        return Ok(());
    }
    if response.deliveries.is_empty() {
        writeln!(out, "no deliveries")?;
        return Ok(());
    }
    for delivery in &response.deliveries {
        writeln!(
            out,
            "{:>6}  {:<10}  {:<16}  {:<14}  {}/{}{}",
            delivery.id,
            state_name(delivery.state),
            truncate(&delivery.destination_name, 16),
            truncate(&delivery.event, 14),
            delivery.attempts,
            delivery.max_attempts,
            if delivery.last_status == 0 {
                String::new()
            } else {
                format!("  http {}", delivery.last_status)
            },
        )?;
        if !delivery.last_error.is_empty() {
            writeln!(out, "        {}", delivery.last_error)?;
        }
        if args.show_payload && !delivery.payload.is_empty() {
            writeln!(out, "        {}", delivery.payload)?;
        }
    }
    Ok(())
}

async fn replay(socket: &Path, args: ReplayArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .replay_delivery(ReplayDeliveryRequest {
            delivery_id: args.delivery_id,
        })
        .await
        .context("ReplayDelivery RPC failed")?
        .into_inner();
    let mut out = std::io::stdout().lock();
    match response.delivery {
        Some(delivery) => writeln!(
            out,
            "delivery {} re-armed ({} -> {}); it is sent on the dispatcher's next tick",
            delivery.id,
            delivery.destination_name,
            state_name(delivery.state)
        )?,
        None => writeln!(out, "delivery {} re-armed", args.delivery_id)?,
    }
    Ok(())
}

/// Run `mail forward <id> --to <destination>`.
///
/// # Errors
/// No daemon, a failed RPC, or an unwritable stdout.
pub async fn forward(socket: &Path, args: ForwardArgs) -> Result<()> {
    // `slack:eng-alerts` -> `eng-alerts`. Only the part before the *first*
    // colon is treated as a prefix, and only when it names a template this
    // build knows: a destination legitimately named `a:b` must not be
    // silently truncated to `b`.
    let destination = strip_kind_prefix(&args.to);
    let channel = connect(socket).await?;
    let response = WebhookServiceClient::new(channel)
        .forward(ForwardMessageRequest {
            message_id: args.message_id,
            destination: destination.to_owned(),
        })
        .await
        .context("Forward RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    let Some(delivery) = response.delivery else {
        anyhow::bail!("the daemon queued a forward but returned nothing about it");
    };
    if args.json {
        writeln!(out, "{}", serde_json::to_string(&delivery_json(&delivery))?)?;
        return Ok(());
    }
    if response.dispatcher_running {
        writeln!(
            out,
            "queued delivery {} to {} (message {}); it is sent on the dispatcher's next tick",
            delivery.id, delivery.destination_name, args.message_id
        )?;
    } else {
        // Never "sent". The delivery is durable and will go out, but not on
        // this daemon as configured, and saying otherwise is the one thing a
        // forward command must not do.
        writeln!(
            out,
            "queued delivery {} to {} (message {}) — but this daemon is NOT running a webhook \
             dispatcher (webhooks.enabled is false), so nothing will be sent until you enable \
             it. The delivery is durable and waits in `mail webhook deliveries`.",
            delivery.id, delivery.destination_name, args.message_id
        )?;
    }
    Ok(())
}

/// Strip a leading `<kind>:` when `<kind>` names a template this build knows.
fn strip_kind_prefix(target: &str) -> &str {
    let target = target.trim();
    for prefix in ["slack:", "generic:", "webhook:"] {
        if let Some(rest) = target.strip_prefix(prefix) {
            return rest;
        }
    }
    target
}

fn write_destination(out: &mut impl Write, destination: &WebhookDestination) -> Result<()> {
    writeln!(
        out,
        "{}  {}  [{}]{}",
        destination.name,
        destination.url,
        template_name(destination.template),
        if destination.enabled {
            ""
        } else {
            "  (disabled)"
        }
    )?;
    writeln!(
        out,
        "  events: {}",
        if destination.events.is_empty() {
            "none (explicit `mail forward` only)".to_owned()
        } else {
            destination
                .events
                .iter()
                .map(|e| event_name(*e).to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        }
    )?;
    writeln!(
        out,
        "  body: {}   signing: {}",
        if destination.include_body {
            "included (redacted)"
        } else {
            "excluded"
        },
        match secret_name(destination.secret_source) {
            None => "unsigned".to_owned(),
            Some(kind) => format!("{kind}:{}", destination.secret_reference),
        }
    )?;
    Ok(())
}

fn destination_json(destination: &WebhookDestination) -> serde_json::Value {
    serde_json::json!({
        "id": destination.id,
        "name": destination.name,
        "url": destination.url,
        "template": template_name(destination.template),
        "events": destination
            .events
            .iter()
            .map(|e| event_name(*e))
            .collect::<Vec<_>>(),
        "include_body": destination.include_body,
        "enabled": destination.enabled,
        "secret_source": secret_name(destination.secret_source),
        "secret_reference": destination.secret_reference,
        "max_attempts": destination.max_attempts,
    })
}

fn delivery_json(delivery: &WebhookDelivery) -> serde_json::Value {
    serde_json::json!({
        "id": delivery.id,
        "destination": delivery.destination_name,
        "event": delivery.event,
        "event_key": delivery.event_key,
        "message_id": delivery.message_id,
        "state": state_name(delivery.state),
        "attempts": delivery.attempts,
        "max_attempts": delivery.max_attempts,
        "next_attempt_at": delivery.next_attempt_at,
        "last_status": delivery.last_status,
        "last_error": delivery.last_error,
        "created_at": delivery.created_at,
        "delivered_at": delivery.delivered_at,
        "payload": delivery.payload,
    })
}

fn template_name(value: i32) -> &'static str {
    match WebhookTemplate::try_from(value) {
        Ok(WebhookTemplate::Slack) => "slack",
        _ => "generic",
    }
}

fn event_name(value: i32) -> &'static str {
    match WebhookEvent::try_from(value) {
        Ok(WebhookEvent::OnNewMessage) => "on_new_message",
        Ok(WebhookEvent::OnLabel) => "on_label",
        Ok(WebhookEvent::OnMove) => "on_move",
        Ok(WebhookEvent::OnRuleMatch) => "on_rule_match",
        Ok(WebhookEvent::OnSyncError) => "on_sync_error",
        Ok(WebhookEvent::Unspecified) | Err(_) => "unknown",
    }
}

fn state_name(value: i32) -> &'static str {
    match WebhookDeliveryState::try_from(value) {
        Ok(WebhookDeliveryState::Pending) => "pending",
        Ok(WebhookDeliveryState::Delivered) => "delivered",
        Ok(WebhookDeliveryState::Failed) => "failed",
        Ok(WebhookDeliveryState::Unspecified) | Err(_) => "unknown",
    }
}

fn secret_name(value: i32) -> Option<&'static str> {
    match WebhookSecretSource::try_from(value) {
        Ok(WebhookSecretSource::Env) => Some("env"),
        Ok(WebhookSecretSource::Command) => Some("command"),
        Ok(WebhookSecretSource::Keychain) => Some("keychain"),
        Ok(WebhookSecretSource::Unspecified) | Err(_) => None,
    }
}

/// First `max` characters, for a fixed-width column.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

async fn connect(socket: &Path) -> Result<crate::client::Client> {
    crate::client::connect(socket).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_prefix_is_stripped_but_a_colon_in_a_name_is_not() {
        assert_eq!(strip_kind_prefix("slack:eng-alerts"), "eng-alerts");
        assert_eq!(strip_kind_prefix("generic:tickets"), "tickets");
        assert_eq!(strip_kind_prefix("eng-alerts"), "eng-alerts");
        // Not a known kind, so it is a name that happens to contain a colon.
        assert_eq!(strip_kind_prefix("team:alerts"), "team:alerts");
        assert_eq!(strip_kind_prefix("  slack:eng  "), "eng");
    }
}
