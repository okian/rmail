//! `mail api ping | reflect | call` — the generic gRPC client (task 42).
//!
//! Three verbs that know nothing about mail. `ping` is the health probe with a
//! latency number attached, `reflect` is "what does this daemon serve", and
//! `call` is the escape hatch that reaches every RPC without a purpose-built
//! verb — the surface prd.md's design invariant implies, since "if gRPC can do
//! it, it is a feature" is only checkable from a shell if a shell can call
//! gRPC.
//!
//! The heavy lifting lives in [`crate::api_call`]; this module is the argument
//! parsing and the rendering.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::{json, Value};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

use crate::format::{self, Classified, ExitCode, OutputFormat};

/// `mail api <action>`.
#[derive(Debug, Subcommand)]
pub enum ApiAction {
    /// Round-trip `grpc.health.v1.Health/Check` and report the latency.
    Ping,
    /// List the services and methods this daemon serves, over gRPC
    /// reflection.
    Reflect {
        /// Print each method's request and response message names too.
        #[arg(long)]
        types: bool,
    },
    /// Call any RPC the daemon serves with a JSON request body.
    ///
    /// The method may be written `MailService.List`,
    /// `rmail.v1.MailService.List` or `rmail.v1.MailService/List`. The body is
    /// proto JSON with the *proto* field names (`page_size`, not `pageSize`);
    /// an unrecognised key is an error rather than something to drop.
    Call {
        /// The method, e.g. `MailService.List`.
        method: String,
        /// The request message as JSON. Omit for a message with no fields.
        #[arg(default_value = "{}")]
        body: String,
        /// Stop a server-streaming method after this many frames.
        #[arg(long, default_value_t = 100)]
        max_frames: usize,
    },
}

/// How long `api call` waits when `--deadline` was not given.
///
/// Long enough for a real query over a large mailbox, short enough that a
/// wedged daemon does not hold a shell forever. `--deadline` overrides it in
/// both directions.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for the health probe, so a wedged daemon cannot hang `mail api
/// ping`.
const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Dispatch `mail api <action>`.
///
/// # Errors
/// Any transport or RPC failure, classified onto [`ExitCode`].
pub async fn run(socket: &Path, action: ApiAction) -> Result<()> {
    match action {
        ApiAction::Ping => ping(socket).await,
        ApiAction::Reflect { types } => reflect(socket, types).await,
        ApiAction::Call {
            method,
            body,
            max_frames,
        } => call(socket, &method, &body, max_frames).await,
    }
}

/// `grpc.health.v1.Health/Check`, timed.
async fn ping(socket: &Path) -> Result<()> {
    let client = crate::client::connect(socket).await?;
    let mut health = HealthClient::new(client);
    let started = std::time::Instant::now();
    let mut request = tonic::Request::new(HealthCheckRequest {
        service: String::new(),
    });
    request.set_timeout(PING_TIMEOUT);
    let response = health
        .check(request)
        .await
        .context("Health/Check RPC failed")?
        .into_inner();
    let elapsed = started.elapsed();

    let status = ServingStatus::try_from(response.status).unwrap_or(ServingStatus::Unknown);
    let value = json!({
        "serving": status == ServingStatus::Serving,
        "status": status.as_str_name(),
        "latency_ms": elapsed.as_secs_f64() * 1000.0,
    });
    match format::current() {
        OutputFormat::Table => {
            println!(
                "{} in {:.1}ms",
                status.as_str_name(),
                elapsed.as_secs_f64() * 1000.0
            );
        }
        OutputFormat::Json => println!("{}", format::to_document(&value)?),
        OutputFormat::Ndjson => println!("{}", format::to_line(&value)?),
    }
    if status != ServingStatus::Serving {
        return Err(Classified::new(
            ExitCode::FailedPrecondition,
            format!(
                "rmaild answered {} rather than SERVING",
                status.as_str_name()
            ),
        ));
    }
    Ok(())
}

/// What the daemon says it serves.
async fn reflect(socket: &Path, types: bool) -> Result<()> {
    let parts = crate::client::connect_parts(socket).await?;
    let deadline = parts.deadline;
    let reflected = crate::api_call::reflect(parts.into_client()?, deadline).await?;

    let mut services: Vec<Value> = Vec::new();
    for service in &reflected.services {
        let prefix = format!("/{service}/");
        let methods: Vec<Value> = reflected
            .catalog
            .methods()
            .iter()
            .filter(|m| m.path.starts_with(&prefix))
            .map(|m| {
                json!({
                    "name": m.path.trim_start_matches(&prefix),
                    "path": m.path,
                    "input_type": m.input_type,
                    "output_type": m.output_type,
                    "server_streaming": m.server_streaming,
                    "client_streaming": m.client_streaming,
                })
            })
            .collect();
        services.push(json!({ "service": service, "methods": methods }));
    }
    let value = json!({ "services": services });

    match format::current() {
        OutputFormat::Table => {
            for service in &services {
                // Every string here came off the wire from the daemon's
                // reflection service, which makes it remote data even though
                // the daemon is trusted: a descriptor set assembled from
                // somewhere else must not be able to drive a terminal.
                println!("{}", crate::terminal_safe(text(&service["service"])));
                for method in service["methods"].as_array().into_iter().flatten() {
                    let streaming = if method["server_streaming"] == json!(true) {
                        " (stream)"
                    } else {
                        ""
                    };
                    if types {
                        println!(
                            "  {}{streaming}  {} -> {}",
                            crate::terminal_safe(text(&method["name"])),
                            crate::terminal_safe(text(&method["input_type"])),
                            crate::terminal_safe(text(&method["output_type"])),
                        );
                    } else {
                        println!(
                            "  {}{streaming}",
                            crate::terminal_safe(text(&method["name"]))
                        );
                    }
                }
            }
        }
        OutputFormat::Json => println!("{}", format::to_document(&value)?),
        // One line per service rather than one document: a listing is a
        // sequence, and `ndjson` is how this binary spells a sequence.
        OutputFormat::Ndjson => {
            for service in &services {
                println!("{}", format::to_line(service)?);
            }
        }
    }
    Ok(())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// `mail api call <Method> <json>`.
async fn call(socket: &Path, method: &str, body: &str, max_frames: usize) -> Result<()> {
    if max_frames == 0 {
        return Err(Classified::new(
            ExitCode::Usage,
            "--max-frames must be at least 1",
        ));
    }
    let arguments: Value = serde_json::from_str(body).map_err(|e| {
        Classified::new(
            ExitCode::Usage,
            format!("the request body is not valid JSON: {e}"),
        )
    })?;

    let parts = crate::client::connect_parts(socket).await?;
    let timeout = parts.deadline.unwrap_or(DEFAULT_CALL_TIMEOUT);
    let token = parts.token.clone();
    // The reflection exchange is itself an authenticated RPC, so it goes
    // through the same interceptor as everything else — and gets its own share
    // of the deadline, or it could hold the process past `--deadline` before
    // the call it is preparing for even starts.
    let reflected = crate::api_call::reflect(
        crate::client::Parts {
            channel: parts.channel.clone(),
            token: token.clone(),
            deadline: parts.deadline,
        }
        .into_client()?,
        parts.deadline,
    )
    .await?;
    let resolved = crate::api_call::resolve(&reflected.catalog, method)?;

    let value = crate::api_call::invoke(
        &parts.channel,
        &crate::api_call::Invocation {
            catalog: &reflected.catalog,
            method: resolved,
            label: method,
            arguments: &arguments,
            max_frames,
            timeout,
            bearer: token.as_deref(),
        },
    )
    .await?;

    match format::current() {
        // There is no table for an arbitrary message, and inventing one would
        // be a rendering nobody could rely on. The default output is the
        // indented JSON document — the same thing `--format json` gives —
        // because this verb's whole purpose is to produce it.
        OutputFormat::Table | OutputFormat::Json => println!("{}", format::to_document(&value)?),
        OutputFormat::Ndjson => {
            // A server-streaming call answers `{frames: [...], ...}`; ndjson
            // means one line per frame, so the envelope is unwrapped here and
            // its truncation flag is reported on stderr, where it cannot be
            // mistaken for a frame.
            match value.get("frames").and_then(Value::as_array) {
                Some(frames) => {
                    for frame in frames {
                        println!("{}", format::to_line(frame)?);
                    }
                    if value.get("truncated") == Some(&json!(true)) {
                        eprintln!(
                            "warning: the stream was truncated ({})",
                            crate::terminal_safe(text(&value["reason"]))
                        );
                    }
                }
                None => println!("{}", format::to_line(&value)?),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
