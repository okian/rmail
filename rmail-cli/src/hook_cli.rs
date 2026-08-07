//! `mail hook add/list/test` — the CLI surface for the event-hook
//! dispatcher (`rmail_core::hooks`, `rmaild::HookApi`).
//!
//! # `add` edits the config file directly; it is not an RPC
//!
//! Hooks are config-driven — there is no `CreateHook` RPC (see
//! `HookService`'s own proto/module docs for why: hook definitions belong
//! in the master TOML alongside every other settings table, not in a
//! database this surface would then need to keep in sync with it). `mail
//! hook add` therefore does the only thing that makes sense for a
//! config-driven feature: it appends a new `[[hooks.hooks]]` block to the
//! operator's own config file (`$RMAIL_CONFIG`, defaulting to
//! `~/.config/rmail/config.toml`) and validates the *result* parses before
//! writing anything, rather than talking to a running daemon at all. A
//! hook added this way takes effect the next time `rmaild` (re)starts —
//! this command prints that reminder rather than silently implying an
//! immediate effect.
//!
//! Only ever *appends* a brand-new array-of-tables block, at the end of the
//! file, and never touches a line above it: TOML does not require an
//! `[[array.of.tables]]` block to sit next to anything else naming the same
//! array, so this is always syntactically legal regardless of what the rest
//! of the file already contains — no existing key, table, comment, or
//! formatting the operator wrote by hand is parsed, reordered, or rewritten.
//! [`toml_string`] is this file's own minimal TOML basic-string escaper,
//! not a dependency on `toml`/`toml_edit`: appending five or six scalar
//! key/value pairs is the entire serialization surface this command needs.
//!
//! The write itself is crash-safe (write to a process-unique sibling temp
//! path, then rename into place) but **not** safe against two genuinely
//! concurrent `mail hook add` invocations: both read the same "existing"
//! content before either writes, so the second `rename` simply wins and the
//! first invocation's addition is lost. That is an inherent read-modify-
//! write race any config-file editor without a lock file has — the
//! process-unique temp name only rules out the more acute failure of two
//! invocations colliding on the *same* temp path mid-write.
//!
//! `list`/`test` are the genuine RPCs (`HookService.ListHooks`/`TestHook`)
//! — those read/exercise whatever the *running* daemon already loaded, so
//! they go over the wire like every other `mail` verb.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use rmail_proto::v1::hook_service_client::HookServiceClient;
use rmail_proto::v1::{HookEvent as ProtoHookEvent, ListHooksRequest, TestHookRequest};

/// `mail hook <action>`.
#[derive(Debug, Subcommand)]
pub enum HookAction {
    /// Add a hook to the local config file (see this module's own docs on
    /// why this edits the file directly rather than calling an RPC).
    Add {
        /// Event this hook fires on.
        event: EventArg,
        /// Unique hook name (what `list`/`test` address it by).
        #[arg(long)]
        name: String,
        /// Per-hook execution timeout (e.g. "10s"); falls back to
        /// `hooks.default_timeout` when omitted.
        #[arg(long)]
        timeout: Option<String>,
        /// Add the hook disabled — listed by `mail hook list`, runnable via
        /// `mail hook test`, but never fired automatically.
        #[arg(long)]
        disabled: bool,
        /// The command to run and its arguments, e.g.
        /// `-- /usr/local/bin/notify --loud`. Never interpreted by a shell
        /// this command introduces — an operator who wants shell features
        /// names `/bin/sh` here with `-c '...'`, the same convention
        /// cron/systemd use.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List every configured hook, enabled or not (`HookService.ListHooks`).
    List,
    /// Run a hook now against the live daemon (`HookService.TestHook`).
    Test {
        /// Hook name, as shown by `mail hook list`.
        name: String,
        /// Event JSON to pipe to the hook's stdin instead of a synthetic
        /// sample event.
        #[arg(long = "event-json")]
        event_json: Option<String>,
    },
}

/// The fixed hook-event vocabulary as a `clap` value — mirrors
/// `rmail_core::config::HookEvent`'s wire form exactly (`on_new_message`,
/// `on_label`, `on_move`, `on_rule_match`, `on_sync_error`). Its own type,
/// not a reuse of the core enum, because `rmail-core` does not and should
/// not depend on `clap`.
// The shared `On*` prefix is the PRD/config vocabulary itself
// (`on_new_message`, `on_label`, ...), not an accidental naming collision
// `enum_variant_names` exists to catch — stripping it would make the CLI's
// own `--help` output name something other than what the config file and
// proto actually call these events.
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
    /// The TOML/wire string this event serializes as — must match
    /// `rmail_core::config::HookEvent`'s `serde(rename_all = "snake_case")`
    /// output exactly, since this is what gets written straight into the
    /// operator's config file.
    const fn wire(self) -> &'static str {
        match self {
            Self::OnNewMessage => "on_new_message",
            Self::OnLabel => "on_label",
            Self::OnMove => "on_move",
            Self::OnRuleMatch => "on_rule_match",
            Self::OnSyncError => "on_sync_error",
        }
    }
}

/// Dispatch `mail hook <action>`.
pub async fn run(socket: &Path, action: HookAction) -> Result<()> {
    match action {
        HookAction::Add {
            event,
            name,
            timeout,
            disabled,
            command,
        } => {
            // `add` is synchronous, blocking `std::fs` I/O — harmless in
            // practice for a one-shot CLI process with nothing else on this
            // runtime, but `spawn_blocking` is the correct place for it
            // regardless of how small the cost is here.
            tokio::task::spawn_blocking(move || add(event, name, timeout, disabled, command))
                .await
                .context("the hook-add task panicked")?
        }
        HookAction::List => list(socket).await,
        HookAction::Test { name, event_json } => test(socket, name, event_json).await,
    }
}

// ---------------------------------------------------------------------------
// add: a local config-file edit, no daemon required
// ---------------------------------------------------------------------------

fn add(
    event: EventArg,
    name: String,
    timeout: Option<String>,
    disabled: bool,
    command_and_args: Vec<String>,
) -> Result<()> {
    let Some((command, args)) = command_and_args.split_first() else {
        bail!(
            "provide a command to run after `--`, e.g. \
             `mail hook add on_new_message --name notify -- /usr/local/bin/notify`"
        );
    };
    if let Some(timeout) = &timeout {
        rmail_core::config::parse_human_duration(timeout)
            .map_err(|e| anyhow::anyhow!("invalid --timeout {timeout:?}: {e}"))?;
    }

    let path = rmail_core::config_path_from_env();
    // A missing file is the normal first-run state (defaults apply, same as
    // `Config::load_or_default`) and becomes an empty base to append to.
    // Any *other* I/O error (permission denied, the path is a directory, a
    // transient EIO) must not be silently swallowed the same way -- doing
    // so would treat "could not read the real config" as "there is no
    // config," and the write below would then replace the operator's real
    // (unread) file with one containing only this new hook.
    let existing = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading existing config at {}", path.display()));
        }
    };

    // Fail fast on both a broken existing file and a name collision, before
    // touching disk at all.
    let current = rmail_core::Config::from_toml_str(&existing).with_context(|| {
        format!(
            "existing config at {} does not parse; fix it before adding a hook",
            path.display()
        )
    })?;
    if current.hooks.hooks.iter().any(|h| h.name == name) {
        bail!(
            "a hook named {name:?} already exists in {} — pick another name \
             or edit the file directly",
            path.display()
        );
    }

    let mut block = String::from("\n[[hooks.hooks]]\n");
    block.push_str(&format!("name = {}\n", toml_string(&name)));
    block.push_str(&format!("event = {}\n", toml_string(event.wire())));
    block.push_str(&format!("command = {}\n", toml_string(command)));
    if !args.is_empty() {
        let items: Vec<String> = args.iter().map(|a| toml_string(a)).collect();
        block.push_str(&format!("args = [{}]\n", items.join(", ")));
    }
    if disabled {
        block.push_str("enabled = false\n");
    }
    if let Some(timeout) = &timeout {
        block.push_str(&format!("timeout = {}\n", toml_string(timeout)));
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&block);

    // Round-trip validated before anything touches disk: a hook whose
    // fields somehow produced invalid TOML (or an invalid config value —
    // `parse_human_duration` above already checked `--timeout`'s syntax,
    // but this also catches anything else) must not corrupt the operator's
    // existing, working config.
    rmail_core::Config::from_toml_str(&updated).context(
        "the hook to add would produce an invalid configuration file; nothing was written",
    )?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    // The existing file's permissions (if any), so the rewrite below does
    // not silently loosen them to the umask default -- a config an operator
    // deliberately locked down (`chmod 600`) must stay that way after
    // `mail hook add` touches it.
    let original_permissions = std::fs::metadata(&path).ok().map(|m| m.permissions());

    // Write to a sibling temp path unique to this process (so two
    // concurrent `mail hook add` invocations cannot collide on the exact
    // same temp file -- see the module docs on the residual read-modify-
    // write race this does *not* close) and rename into place, so a crash
    // mid-write cannot leave the operator's config half-written.
    let tmp_path: PathBuf = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, &updated)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    if let Some(permissions) = original_permissions {
        // Best-effort: a failure here must not abort an otherwise-successful
        // write, and the file lands with the ordinary umask-derived mode
        // rather than none at all.
        let _ = std::fs::set_permissions(&tmp_path, permissions);
    }
    std::fs::rename(&tmp_path, &path).with_context(|| format!("replacing {}", path.display()))?;

    println!(
        "added hook {name:?} ({}) to {}",
        event.wire(),
        path.display()
    );
    println!("restart rmaild for the new hook to take effect");
    Ok(())
}

/// Render `value` as a TOML basic-string literal — quoted, with the minimal
/// escaping TOML's basic-string grammar requires. See the module docs for
/// why this exists instead of a `toml`/`toml_edit` dependency.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// list / test: real RPCs against the running daemon
// ---------------------------------------------------------------------------

async fn hook_client(socket: &Path) -> Result<HookServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(HookServiceClient::new(channel))
}

async fn list(socket: &Path) -> Result<()> {
    let response = hook_client(socket)
        .await?
        .list_hooks(ListHooksRequest {})
        .await
        .context("ListHooks RPC failed")?
        .into_inner();

    if response.hooks.is_empty() {
        println!("no hooks configured");
        return Ok(());
    }
    for hook in response.hooks {
        let status = if hook.enabled { "enabled" } else { "disabled" };
        let event = ProtoHookEvent::try_from(hook.event)
            .map(|e| {
                e.as_str_name()
                    .trim_start_matches("HOOK_EVENT_")
                    .to_ascii_lowercase()
            })
            .unwrap_or_else(|_| format!("unknown({})", hook.event));
        let rest = if hook.args.is_empty() {
            hook.command
        } else {
            format!("{} {}", hook.command, hook.args.join(" "))
        };
        println!("{:<20} {:<8} {:<16} {}", hook.name, status, event, rest);
    }
    Ok(())
}

async fn test(socket: &Path, name: String, event_json: Option<String>) -> Result<()> {
    let response = hook_client(socket)
        .await?
        .test_hook(TestHookRequest { name, event_json })
        .await
        .context("TestHook RPC failed")?
        .into_inner();

    if response.timed_out {
        println!("result:   timed out (killed)");
    } else if response.cancelled {
        println!("result:   cancelled (daemon shutting down)");
    } else {
        match response.exit_code {
            Some(code) => println!("result:   exit code {code}"),
            None => println!("result:   could not spawn the command"),
        }
    }
    println!("duration: {} ms", response.duration_ms);
    if !response.stdout.is_empty() {
        println!("\nstdout:\n{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        println!("\nstderr:\n{}", response.stderr);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
