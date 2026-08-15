//! `mail mcp serve` — expose the daemon's whole gRPC surface to an AI agent
//! over the Model Context Protocol (task 53).
//!
//! This module is a *launcher*, not an implementation. Every tool, its schema,
//! its scope gate and its dispatch live in `rmaild::mcp`, derived from the
//! compiled descriptor set; what happens here is connecting to the daemon,
//! deciding which scopes this connection claims, and picking a transport.
//! Keeping it that thin is what makes "a new RPC yields a new tool with zero
//! extra code" true for this binary as well: there is nothing in this file
//! that names an RPC.
//!
//! # One hop, in-process
//!
//! The MCP server holds one `tonic::transport::Channel` to the daemon's
//! existing Unix socket and dispatches every tool call straight down it. There
//! is no second socket, no local proxy, and no subprocess between the agent
//! and the daemon — the adapter is a client of the API in the same process,
//! exactly as the TUI is.
//!
//! # Declared scopes are a guardrail, not a sandbox
//!
//! `--scope` decides which tools are listed and which calls this process is
//! willing to send. It is genuinely useful — pointing an agent at a read-only
//! surface stops it reaching for `delete_message` at all — but over the local
//! Unix socket it is the **only** thing narrowing the agent, and that is worth
//! being blunt about.
//!
//! `rmaild`'s auth layer grants `admin` to a Unix-socket peer whose uid
//! matches the daemon's *before it looks at the `authorization` header at
//! all* (see `rmaild::auth`'s "Two principals"). So on the socket this command
//! connects to:
//!
//! - `--token` is **not consulted**. Minting a `mail.read` token and passing
//!   it here does not narrow anything server-side: the daemon still sees the
//!   socket's owner and still grants admin. The token matters on the TCP
//!   listener, where there is no peer uid to trust.
//! - `--scope` therefore constrains this process and nothing else. An agent
//!   driving this server cannot call what is not listed and not authorized
//!   here; anything else with access to the socket is unaffected.
//!
//! That makes `--scope` the thing to get right rather than a formality, and it
//! makes this process — not the daemon — the component whose compromise would
//! matter. `rmaild/tests/mcp_server.rs` pins the behaviour so it cannot change
//! silently, and points at where to update these words if it ever does.
//!
//! # `--read-only` says something `--scope mail.read` does not
//!
//! Scope answers "what may this caller do"; effect answers "does calling it
//! change anything". They are decided in different tables and one row makes
//! them differ — `log_search_feedback` mutates at `mail.read`, on the argument
//! that a read-only agent is exactly the one that should be able to improve
//! its own future searches. `--scope mail.read` therefore lists it, honestly,
//! because the daemon would run it. `--read-only` is the stricter statement,
//! and it withholds that tool as well; because this process then also *refuses*
//! it, the shorter listing is still an accurate description of the surface
//! rather than an under-report. `rmaild::mcp::tools` carries the full argument,
//! including what the flag deliberately does not promise (search still writes
//! to the local learning log, which is `search.learning`'s job to turn off).

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rmail_core::auth::Scope;
use rmaild::mcp::{self, CallLimits, McpServer, Mutations, Principal};
use tokio_util::sync::CancellationToken;

/// `mail mcp ...`.
#[derive(Debug, clap::Subcommand)]
pub enum McpAction {
    /// Serve the projected tool surface to an MCP client.
    Serve(ServeArgs),
}

/// `mail mcp serve`.
#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Speak MCP over stdin/stdout — how `claude` and most MCP clients launch
    /// a server. Mutually exclusive with `--sse`.
    #[arg(long, conflicts_with = "sse")]
    stdio: bool,

    /// Speak MCP over HTTP+SSE on `--addr` instead of stdio.
    #[arg(long)]
    sse: bool,

    /// Address for `--sse`. Loopback only: the endpoint is unauthenticated
    /// and hands every caller the scopes this server was started with.
    #[arg(long, default_value = "127.0.0.1:8909")]
    addr: SocketAddr,

    /// Bearer token presented to the daemon on every call. Without one the
    /// daemon's Unix-peer path applies, which grants the owning user admin.
    ///
    /// Prefer the RMAIL_TOKEN environment variable: a secret passed on the
    /// command line is visible in `ps` for the life of the process and lands
    /// in shell history. Note also that over the local Unix socket the daemon
    /// never reads this — see this command's own docs.
    #[arg(long, env = "RMAIL_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// The scopes this connection claims, for filtering the tool list:
    /// `mail.read`, `mail.write`, `mail.send`, `ai.invoke`, `automation`,
    /// `admin`. Repeatable and/or comma-separated. Required with `--token`,
    /// because a bearer secret is opaque — this process cannot see what it
    /// was minted with. Read this module's docs before relying on it.
    #[arg(long = "scope", value_delimiter = ',')]
    scopes: Vec<String>,

    /// The most stream frames one tool call drains before answering with a
    /// truncated prefix.
    #[arg(long, default_value_t = 200)]
    max_frames: usize,

    /// Wall-clock budget for a single tool call ("30s", "2m"), also sent as
    /// the gRPC deadline so the daemon stops work nobody will read.
    #[arg(long, default_value = "60s")]
    timeout: String,

    /// Withhold every tool that changes mail, spends at a model provider, or
    /// produces something that can — whatever `--scope` allows.
    ///
    /// `--scope mail.read` already refuses the mutations that sit behind
    /// `mail.write`, but it is *scope* filtering, and one capability
    /// deliberately mutates at read scope: `log_search_feedback` writes the
    /// click data that improves your own later searches. This flag is the
    /// stricter, effect-based statement, and it withholds that one too. A
    /// withheld tool is not merely hidden: this process refuses to send it, so
    /// the listing still describes the surface exactly.
    ///
    /// Not the same as "writes nothing at all": search is read-only by this
    /// measure and still appends to the local learning log — set
    /// `search.learning = false` for that. See `rmaild::mcp::tools`.
    #[arg(long)]
    read_only: bool,

    /// Print the projected tool surface and exit, instead of serving. The
    /// listing is filtered exactly as an MCP client would see it.
    #[arg(long)]
    list: bool,
}

/// Run `mail mcp <action>`.
///
/// # Errors
///
/// Fails if neither transport was chosen, if `--token` was given without
/// `--scope`, if a scope string does not parse, if the daemon is unreachable,
/// or if the transport itself fails.
pub async fn run(socket: &Path, action: McpAction) -> Result<()> {
    match action {
        McpAction::Serve(args) => serve(socket, args).await,
    }
}

async fn serve(socket: &Path, args: ServeArgs) -> Result<()> {
    // stderr, never stdout: on `--stdio` the standard output stream *is* the
    // JSON-RPC channel, and one stray line on it desynchronizes the client.
    // Installed here rather than in `main` so no other verb's output changes.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    if !args.stdio && !args.sse && !args.list {
        bail!(
            "choose a transport: --stdio (for an MCP client that launches this process) or --sse"
        );
    }
    let scopes = resolve_scopes(&args)?;
    let timeout = rmail_core::config::parse_human_duration(&args.timeout)
        .map_err(|e| anyhow::anyhow!("invalid --timeout: {e}"))?;
    if args.max_frames == 0 {
        bail!("--max-frames must be at least 1; a call that drains no frames returns nothing");
    }

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;

    let cancel = CancellationToken::new();
    let server = McpServer::new(
        channel,
        principal(&args, scopes),
        CallLimits {
            max_frames: args.max_frames,
            timeout,
        },
        cancel.clone(),
    )
    .context("projecting the gRPC surface as MCP tools")?;

    if args.list {
        return print_surface(&server);
    }

    // Ctrl-C ends the session cleanly: an in-flight tool call is cancelled
    // (which drops its gRPC stream, which tells the daemon to stop) rather
    // than the process dying mid-write.
    let signal = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        }
    });

    let result = if args.sse {
        mcp::serve_sse(server, args.addr, cancel.clone()).await
    } else {
        mcp::serve_stdio(server, cancel.clone()).await
    };
    cancel.cancel();
    signal.abort();
    result.map_err(anyhow::Error::from)
}

/// Who this connection claims to be, and what it will offer.
///
/// Split out of [`serve`] so the one line that turns `--read-only` into a
/// policy is reachable from a test: it is the whole user-facing surface of
/// "a read-only token's tool list contains only read tools", and inverting it
/// would otherwise be invisible to every test in `rmaild::mcp`.
fn principal(args: &ServeArgs, scopes: Vec<Scope>) -> Principal {
    Principal {
        scopes,
        bearer: args.token.clone(),
        mutations: if args.read_only {
            Mutations::Withheld
        } else {
            Mutations::AsScoped
        },
    }
}

/// The scopes this connection claims — see this module's docs on why the
/// two cases differ.
fn resolve_scopes(args: &ServeArgs) -> Result<Vec<Scope>> {
    if args.scopes.is_empty() {
        if args.token.is_some() {
            bail!(
                "--token was given but --scope was not. A bearer secret is opaque, so this \
                 process cannot tell what the token grants and would either advertise tools the \
                 daemon will refuse or hide tools it would allow. Pass the scopes the token was \
                 minted with, e.g. --scope mail.read,ai.invoke."
            );
        }
        if args.sse {
            // Defaulting `--sse` to admin would put the whole surface behind a
            // TCP port with no authentication, reachable by anything running
            // as any user on this host. `--stdio` has a parent process that
            // chose to launch it; a port does not, so it must be asked.
            bail!(
                "--sse needs an explicit --scope. Unlike --stdio, the SSE endpoint has no \
                 parent process that chose to start it: it is a TCP port anything on this host \
                 can connect to, so it must not silently inherit the admin the daemon grants \
                 this socket's owner. State what the agent may do, e.g. --scope mail.read."
            );
        }
        // `--stdio` with no token: the daemon's Unix-peer path decides, and
        // for the socket's owner that is `admin`. Claiming anything narrower
        // here would hide tools the daemon would in fact run.
        return Ok(vec![Scope::Admin]);
    }
    args.scopes
        .iter()
        .map(|raw| {
            raw.parse::<Scope>()
                .map_err(|e| anyhow::anyhow!("invalid --scope {raw:?}: {e}"))
        })
        .collect()
}

/// `--list`: the tool surface exactly as this connection would advertise it.
///
/// Every string printed here is a compile-time constant out of the parity
/// registry (a tool name, an RPC path) — no mail and no model output reaches
/// this listing, which is why it needs none of `terminal_safe`'s treatment.
fn print_surface(server: &McpServer) -> Result<()> {
    let visible = server.visible_tools();
    println!(
        "{} of {} tools visible to this connection ({})",
        visible.len(),
        server.surface().tools().len(),
        server.visibility().mutations().label()
    );
    for tool in visible {
        println!(
            "{:<28} {:<7} {}",
            tool.name(),
            if tool.effect() == rmail_core::parity::Effect::Read {
                "read"
            } else {
                "mutate"
            },
            tool.rpc()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// Parse a `mail mcp serve` invocation the way clap will at runtime, so
    /// the flag's *name* is under test alongside what it does.
    #[derive(clap::Parser)]
    struct Probe {
        #[command(flatten)]
        args: ServeArgs,
    }

    fn parse(argv: &[&str]) -> ServeArgs {
        Probe::parse_from(std::iter::once("serve").chain(argv.iter().copied())).args
    }

    /// The one line that turns the flag into a policy, both ways round.
    ///
    /// Without this, inverting the condition passes every test in
    /// `rmaild::mcp` — the whole projection can be correct while the only
    /// route a user has to it is backwards.
    #[test]
    fn read_only_is_what_withholds_the_mutating_tools() {
        let with = parse(&["--stdio", "--read-only"]);
        assert_eq!(
            principal(&with, vec![Scope::MailRead]).mutations,
            Mutations::Withheld
        );

        let without = parse(&["--stdio"]);
        assert_eq!(
            principal(&without, vec![Scope::MailRead]).mutations,
            Mutations::AsScoped,
            "the default must stay the surface the daemon actually accepts"
        );
    }

    /// The flag narrows the effect policy and nothing else: it must not
    /// quietly change the scopes or drop the bearer token.
    #[test]
    fn read_only_changes_nothing_but_the_effect_policy() {
        let args = parse(&["--stdio", "--read-only", "--scope", "mail.read,ai.invoke"]);
        let principal = principal(&args, resolve_scopes(&args).expect("scopes parse"));
        assert_eq!(principal.scopes, vec![Scope::MailRead, Scope::AiInvoke]);
        assert!(principal.bearer.is_none());
    }

    /// The guardrails task 53's reviewer put in place, pinned from here too:
    /// a TCP port and an opaque token both have to be told what they may do.
    #[test]
    fn read_only_does_not_excuse_an_explicit_scope() {
        assert!(
            resolve_scopes(&parse(&["--sse", "--read-only"])).is_err(),
            "--sse must still demand an explicit --scope"
        );
        assert!(
            resolve_scopes(&parse(&[
                "--stdio",
                "--read-only",
                "--token",
                "rmail_tok_x"
            ]))
            .is_err(),
            "--token must still demand an explicit --scope"
        );
        // ...and `--stdio` alone still claims admin, rather than --read-only
        // being mistaken for a scope.
        assert_eq!(
            resolve_scopes(&parse(&["--stdio", "--read-only"])).expect("scopes"),
            vec![Scope::Admin]
        );
    }
}
