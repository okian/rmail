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
//! all* (see `rmaild::auth`'s "Two principals") — **unless**
//! `client_auth.require_for_local` is set, in which case that shortcut is off
//! for every caller, this one included, and `resolve_principal` falls back to
//! whatever `mail auth login` cached instead (or refuses to start the server
//! at all, loudly, if nothing is cached — see that function's own docs).
//! Assume the shortcut applies for the rest of this section; it is the
//! default. So on the socket this command connects to:
//!
//! - `--token` (the *global* flag since task 42 — this verb no longer declares
//!   one of its own, because two arguments with one id are merged by `clap`
//!   rather than reported) is **not consulted**. Minting a `mail.read` token
//!   and passing it here does not narrow anything server-side: the daemon
//!   still sees the socket's owner and still grants admin. The token matters
//!   on the TCP listener, where there is no peer uid to trust.
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
use rmail_proto::v1::client_auth_service_client::ClientAuthServiceClient;
use rmail_proto::v1::AuthStatusRequest;
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

    /// Speak MCP over HTTP+SSE on `--sse-addr` instead of stdio.
    #[arg(long)]
    sse: bool,

    /// Listen address for `--sse`. Loopback only: the endpoint is
    /// unauthenticated and hands every caller the scopes this server was
    /// started with.
    ///
    /// Named `--sse-addr`, not `--addr`: the global `--addr` says where the
    /// *daemon* is, and two arguments with the same id are merged by `clap`
    /// rather than reported as a conflict (see `format`'s
    /// `no_subcommand_shadows_the_global_format_flag`) — so `mail --addr
    /// host:port mcp serve --sse` would have silently made this server bind
    /// the daemon's address.
    #[arg(long = "sse-addr", id = "sse_addr", default_value = "127.0.0.1:8909")]
    sse_addr: SocketAddr,

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
    let timeout = rmail_core::config::parse_human_duration(&args.timeout)
        .map_err(|e| anyhow::anyhow!("invalid --timeout: {e}"))?;
    if args.max_frames == 0 {
        bail!("--max-frames must be at least 1; a call that drains no frames returns nothing");
    }

    // The bare channel, not the intercepted one: this verb hands the
    // connection to `McpServer`, which attaches the principal's own bearer to
    // every projected call (see `principal` below). Layering the global
    // `--token` interceptor underneath as well would put two `authorization`
    // headers on each request.
    let channel = crate::client::connect_parts(socket).await?.channel;
    let (bearer, scopes) =
        resolve_principal(socket, &args, channel.clone(), crate::client::bearer()).await?;

    let cancel = CancellationToken::new();
    let server = McpServer::new(
        channel,
        principal(&args, scopes, bearer),
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
        mcp::serve_sse(server, args.sse_addr, cancel.clone()).await
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
/// `bearer` is the global `--token`/`$RMAIL_TOKEN`, passed in rather than
/// read here: this verb used to declare a second `--token` of its own, which
/// `clap` merged with the global one by value-source precedence rather than
/// reporting as a conflict. One declaration, one reader, and a parameter the
/// tests can set.
fn principal(args: &ServeArgs, scopes: Vec<Scope>, bearer: Option<String>) -> Principal {
    Principal {
        scopes,
        bearer,
        mutations: if args.read_only {
            Mutations::Withheld
        } else {
            Mutations::AsScoped
        },
    }
}

/// The bearer this connection presents, and the scopes it claims — the pure
/// decision, given whatever [`resolve_principal`] already learned about the
/// environment. Split out so the usage-error messages (the part worth
/// pinning in a test) need no running daemon to exercise, even though the
/// full resolution now sometimes does.
///
/// `local_login_required`/`cached_bearer` only matter for the `--stdio`,
/// no-`--token`, no-`--scope` case; every other branch ignores them, which is
/// exactly why [`resolve_principal`] skips the `AuthStatus` round trip and
/// the Keychain read for every branch but that one.
fn decide_principal(
    args: &ServeArgs,
    explicit_bearer: Option<&str>,
    local_login_required: bool,
    cached_bearer: Option<&str>,
) -> Result<(Option<String>, Vec<Scope>)> {
    if !args.scopes.is_empty() {
        let scopes = args
            .scopes
            .iter()
            .map(|raw| {
                raw.parse::<Scope>()
                    .map_err(|e| anyhow::anyhow!("invalid --scope {raw:?}: {e}"))
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((explicit_bearer.map(str::to_owned), scopes));
    }
    if explicit_bearer.is_some() {
        bail!(
            "--token was given but --scope was not. A bearer secret is opaque, so this process \
             cannot tell what the token grants and would either advertise tools the daemon will \
             refuse or hide tools it would allow. Pass the scopes the token was minted with, \
             e.g. --scope mail.read,ai.invoke."
        );
    }
    if args.sse {
        // Defaulting `--sse` to admin would put the whole surface behind a
        // TCP port with no authentication, reachable by anything running as
        // any user on this host. `--stdio` has a parent process that chose to
        // launch it; a port does not, so it must be asked.
        bail!(
            "--sse needs an explicit --scope. Unlike --stdio, the SSE endpoint has no parent \
             process that chose to start it: it is a TCP port anything on this host can connect \
             to, so it must not silently inherit the admin the daemon grants this socket's \
             owner. State what the agent may do, e.g. --scope mail.read."
        );
    }
    if !local_login_required {
        // Unchanged from before `client_auth` existed: the daemon's
        // Unix-peer path decides, and for the socket's owner that is
        // `admin`. Claiming anything narrower here would hide tools the
        // daemon would in fact run.
        return Ok((None, vec![Scope::Admin]));
    }

    // Peer trust alone is not enough here. A session `mail auth login`
    // cached is known, by construction, to carry exactly `Scope::Admin` —
    // `rmaild::client_auth_service::login_password` always mints that one
    // scope — so, unlike an operator-supplied `--token` of unknown
    // provenance, it can stand in for the assumption above without an
    // explicit `--scope`.
    match cached_bearer {
        Some(token) => Ok((Some(token.to_owned()), vec![Scope::Admin])),
        None => bail!(
            "this daemon requires client_auth login even for local callers \
             (client_auth.require_for_local = true), and no session is cached for this socket. \
             Run `mail auth login` first, or pass --token/--scope explicitly."
        ),
    }
}

/// [`decide_principal`], filling in `local_login_required`/`cached_bearer`
/// with a real `AuthStatus` RPC and the local session cache — but only when
/// the decision actually depends on them (an explicit `--scope`, an explicit
/// `--token`, or `--sse` are all decidable with no daemon call at all; see
/// `decide_principal`'s own early returns).
///
/// # Errors
///
/// Everything [`decide_principal`] can return, plus a mapped `AuthStatus`
/// RPC failure.
async fn resolve_principal(
    socket: &Path,
    args: &ServeArgs,
    channel: tonic::transport::Channel,
    explicit_bearer: Option<String>,
) -> Result<(Option<String>, Vec<Scope>)> {
    if !args.scopes.is_empty() || explicit_bearer.is_some() || args.sse {
        return decide_principal(args, explicit_bearer.as_deref(), false, None);
    }

    // `--addr` has no Unix-peer trust to ask about at all: an explicit
    // `--token` is the only way to reach a remote daemon here, and that path
    // already returned above.
    let local_login_required = if crate::client::remote_addr().is_some() {
        false
    } else {
        let mut auth = ClientAuthServiceClient::new(channel);
        auth.auth_status(AuthStatusRequest {})
            .await
            .context("AuthStatus RPC failed")?
            .into_inner()
            .local_login_required
    };
    // Off the runtime — see `client::connect_parts`'s identical guard on the
    // same blocking Keychain call for why.
    let socket_owned = socket.to_path_buf();
    let cached = tokio::task::spawn_blocking(move || crate::session::load(&socket_owned))
        .await
        .unwrap_or_default();
    decide_principal(
        args,
        None,
        local_login_required,
        cached.as_ref().map(|session| session.token.as_str()),
    )
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
            principal(&with, vec![Scope::MailRead], None).mutations,
            Mutations::Withheld
        );

        let without = parse(&["--stdio"]);
        assert_eq!(
            principal(&without, vec![Scope::MailRead], None).mutations,
            Mutations::AsScoped,
            "the default must stay the surface the daemon actually accepts"
        );
    }

    /// The flag narrows the effect policy and nothing else: it must not
    /// quietly change the scopes or drop the bearer token.
    #[test]
    fn read_only_changes_nothing_but_the_effect_policy() {
        let args = parse(&["--stdio", "--read-only", "--scope", "mail.read,ai.invoke"]);
        let (bearer, scopes) = decide_principal(&args, None, false, None).expect("scopes parse");
        let principal = principal(&args, scopes, bearer);
        assert_eq!(principal.scopes, vec![Scope::MailRead, Scope::AiInvoke]);
        assert!(principal.bearer.is_none());
    }

    /// The guardrails task 53's reviewer put in place, pinned from here too:
    /// a TCP port and an opaque token both have to be told what they may do.
    #[test]
    fn read_only_does_not_excuse_an_explicit_scope() {
        assert!(
            decide_principal(&parse(&["--sse", "--read-only"]), None, false, None).is_err(),
            "--sse must still demand an explicit --scope"
        );
        assert!(
            decide_principal(
                &parse(&["--stdio", "--read-only"]),
                // The global `--token`, now passed in rather than declared
                // here — see `principal`.
                Some("rmail_tok_x"),
                false,
                None,
            )
            .is_err(),
            "a bearer token must still demand an explicit --scope"
        );
        // ...and `--stdio` alone still claims admin, rather than --read-only
        // being mistaken for a scope — when the daemon does not require login
        // even for local peers (the default; `client_auth.require_for_local`
        // is the case the next two tests cover).
        assert_eq!(
            decide_principal(&parse(&["--stdio", "--read-only"]), None, false, None)
                .expect("scopes"),
            (None, vec![Scope::Admin])
        );
    }

    /// `client_auth.require_for_local`, the case peer trust alone cannot
    /// answer: a cached session stands in for it, silently, because a
    /// `LoginPassword`-minted session is known to carry exactly
    /// `Scope::Admin` — see `decide_principal`'s own docs.
    #[test]
    fn require_for_local_uses_the_cached_session_as_admin() {
        let args = parse(&["--stdio"]);
        assert_eq!(
            decide_principal(&args, None, true, Some("rmail_tok_cached")).expect("scopes"),
            (Some("rmail_tok_cached".to_owned()), vec![Scope::Admin])
        );
    }

    /// The same case with nothing cached: this must refuse loudly at
    /// startup, not claim `admin` on the strength of a peer-uid check the
    /// daemon has said it will not honour, and leave every tool call to fail
    /// with `UNAUTHENTICATED` one at a time instead.
    #[test]
    fn require_for_local_with_no_cached_session_refuses() {
        let args = parse(&["--stdio"]);
        let err = decide_principal(&args, None, true, None)
            .expect_err("no credential should back an admin claim");
        assert!(
            err.to_string().contains("mail auth login"),
            "the refusal should say how to fix it: {err}"
        );
    }
}
