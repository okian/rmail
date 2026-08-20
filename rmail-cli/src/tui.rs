//! `mail tui` — the terminal shell: folders, message list, preview
//! (prd.md, "TUI"; task 83).
//!
//! # Shape
//!
//! Four modules, one of which touches a terminal:
//!
//! - [`model`] — [`Model`](model::Model), [`Msg`](model::Msg),
//!   [`Cmd`](model::Cmd) and a pure `update`. Every navigation rule and every
//!   action lives here, and every one of them is tested headlessly.
//! - [`model::drive`] — the event loop: recv, update, dispatch, paint.
//! - [`view`] — ratatui rendering, a pure function of `&Model`.
//! - [`grpc`] — the executor that turns commands into background RPCs.
//!
//! Plus [`term`] (raw mode and the alternate screen, and putting both back)
//! and [`html`] ("open HTML in browser").
//!
//! This file is the wiring: connect, take the terminal, start the input
//! reader, run the loop, give the terminal back.
//!
//! # It is a client, and only a client
//!
//! The TUI holds no database handle and speaks no IMAP. Everything it shows
//! comes from `MailService`/`SyncService`/`AccountService` and everything it
//! changes goes through `MailService`; replies and forwards become drafts via
//! `ComposeService` rather than MIME this crate assembles. prd.md is explicit
//! that "UI components never talk to IMAP directly; they are gRPC clients of
//! `rmaild`", and the practical payoff is that the CLI, the TUI and Claude
//! (over MCP) cannot drift apart — there is one implementation of "archive
//! this message" and all three call it.
//!
//! # Startup
//!
//! prd.md budgets 200 ms for "TUI attach to daemon". Nothing on the path to
//! the first frame does I/O beyond connecting the socket: the model starts
//! empty, the first frame paints the chrome, and the account/folder/message
//! round trips fill it in as they land. Holding the first paint until a
//! folder listing returned would put the daemon's response time — and, on a
//! cold mailbox, the disk — inside a budget that has nothing to do with
//! either.
//!
//! # The one blocking call left on the runtime
//!
//! `terminal.draw` writes escape sequences to stdout synchronously — ratatui
//! has no async backend, and there is no useful one to have: a frame is a
//! few kilobytes to a local tty, bounded by the terminal's size rather than
//! by anything a peer controls. Everything genuinely unbounded (RPCs, the
//! event stream, launching a browser) is on a background task or the
//! blocking pool. This is the deliberate exception, not an oversight.
//!
//! # Why keyboard input is a thread and not a task
//!
//! crossterm's async `EventStream` needs its `event-stream` feature, which
//! would mean depending on crossterm directly — and a direct
//! `crossterm = "0.2x"` line is how a project ends up with two crossterm
//! versions in the tree, one writing the backend's escape sequences and the
//! other toggling raw mode. Everything here comes from `ratatui::crossterm`,
//! which re-exports exactly the version ratatui itself was built against. A
//! dedicated OS thread doing a short blocking `poll` and forwarding into the
//! same channel everything else uses is simpler, has no version coupling, and
//! keeps the blocking read off the runtime entirely. The thread polls with a
//! timeout so it observes the stop flag promptly and does not outlive the
//! session.

pub mod commands;
pub mod config_block;
pub mod form;
pub mod grpc;
pub mod help;
pub mod history;
pub mod html;
pub mod manual;
pub mod model;
pub mod overlays;
pub mod report;
pub mod settings;
pub mod status;
pub mod term;
pub mod theme;
pub mod view;
pub mod whichkey;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use tokio::sync::mpsc;

use grpc::GrpcExec;
use model::{drive, Key, Model, Msg};
use term::{Crossterm, TerminalGuard};

/// How long the input thread waits for a key before re-checking whether the
/// session is over.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How often `keys.toml` is re-read.
///
/// Hot reload is the point (prd.md: "fully rebindable and hot-reloadable"),
/// and a second is the longest a person editing a binding in the next window
/// will believe the file was read. The cost is one `read_to_string` of a
/// few hundred bytes on a thread that does nothing else; see
/// `keymap::file`'s own docs on why this is a poll rather than a filesystem
/// watcher.
const KEYMAP_POLL: Duration = Duration::from_secs(1);

/// `mail tui`.
#[derive(Debug, clap::Args)]
pub struct TuiArgs {
    /// Account to open. Defaults to the first one `mail accounts` lists.
    #[arg(long)]
    account: Option<i64>,
    /// Color theme: `dark` (default), `light`, `mono`, or `high-contrast`.
    ///
    /// A full settings surface for this is task 89's `:set theme`; until
    /// then this is the only way to reach the three built-ins besides
    /// `dark`, the same way `$RMAIL_FORMAT` is the CLI's config path before
    /// anything richer exists.
    #[arg(long, env = "RMAIL_THEME")]
    theme: Option<String>,
}

/// Run the TUI against the daemon at `socket`.
///
/// # Errors
///
/// If the daemon cannot be reached, the terminal cannot be taken over, or a
/// frame cannot be drawn.
pub async fn run(socket: &Path, args: TuiArgs) -> Result<()> {
    // Before the terminal is touched: a connection failure should print a
    // plain error to a normal shell, not flash an alternate screen.
    let exec = GrpcExec::connect(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;

    let guard = TerminalGuard::enter(Arc::new(Crossterm))
        .context("putting the terminal into raw mode / alternate screen")?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("initializing the terminal")?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let input = spawn_input(tx.clone(), Arc::clone(&stop));
    // The model starts on the built-in bindings and the watcher's first poll
    // delivers whatever `keys.toml` says, so startup and every later edit
    // take exactly the same path — there is no "load the keymap" step that
    // could succeed at boot and be wrong afterwards.
    let keys = spawn_keymap_watcher(
        crate::keymap::file::keys_path_from_env(),
        tx.clone(),
        Arc::clone(&stop),
    );

    // Boot is a message like any other, so the first loads follow exactly the
    // same path a key press would — nothing special-cased at startup.
    let _ = tx.send(Msg::Boot);

    let mut model = Model::for_account(args.account);
    apply_theme_arg(&mut model, args.theme.as_deref());
    // Read once, here, rather than polled like `keys.toml`: a history is
    // this session's own record, and a second rmail writing its file while
    // this one is running should not reorder the list under somebody's
    // `<up>`. Unreadable is an empty history, never a startup failure — see
    // `history`'s own docs.
    // `spawn_blocking`, not a direct read: this is filesystem work on a
    // runtime thread, and it happens with the terminal already in raw mode —
    // where a blocking `open` is a wedged session rather than a slow start.
    // The write path goes the same way, in `grpc`.
    let history_path = history::path_from_env();
    let lines = tokio::task::spawn_blocking(move || history::read(&history_path))
        .await
        .unwrap_or_default();
    model.history = history::History::new(lines);

    let result = drive::run_loop(model, &mut rx, &tx, &exec, |model| {
        terminal.draw(|frame| view::render(model, frame))?;
        Ok(())
    })
    .await;

    // Tear down in the reverse order of setup, and unconditionally: `result`
    // is not unwrapped until the terminal is back, so an error mid-session
    // still leaves a usable shell.
    stop.store(true, Ordering::SeqCst);
    exec.shutdown();
    // Either thread can be up to `INPUT_POLL` from noticing the flag, and
    // `JoinHandle::join` is a blocking wait — on the blocking pool, not on a
    // runtime worker. Both are joined rather than detached: a watcher still
    // reading `keys.toml` while the terminal is being restored is a thread
    // this function promised to have finished with.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = input.join();
        keys.join()
    })
    .await;
    // Decoded HTML mail this session wrote to /tmp. Safe to remove now: any
    // browser it was written for opened it long ago.
    html::sweep();
    guard.restore();
    drop(terminal);

    result.map(|_| ())
}

/// Apply `--theme`/`$RMAIL_THEME` to a freshly-constructed model.
///
/// A no-op for `None` (the flag was not given). An unrecognized name is a
/// status line, never a startup failure — the same call this module's docs
/// make about a malformed `keys.toml`: a config typo should not be the
/// difference between reading mail and not. `Msg::Boot`'s handler
/// (`model::dispatch`) is what keeps this notice from being overwritten a
/// moment later by "loading accounts…" — it skips that write when the
/// model is already showing an error.
fn apply_theme_arg(model: &mut Model, name: Option<&str>) {
    let Some(name) = name else { return };
    match theme::ThemeName::from_id(name) {
        Some(named) => model.theme = named.resolve(),
        None => {
            model.status = format!("unknown theme {name:?} — using dark");
            model.level = model::Level::Error;
        }
    }
}

/// Poll `keys.toml` on its own thread, delivering every load — the first one
/// included — as a [`Msg::Keymap`].
///
/// A thread rather than a task for the same reason `spawn_input` is one: the
/// poll does a small blocking file read, and doing that on the runtime would
/// stall every RPC in flight for as long as the filesystem takes.
///
/// A send failure ends the thread rather than retrying: the only way the
/// receiver is gone is that the session is already shutting down.
fn spawn_keymap_watcher(
    path: std::path::PathBuf,
    tx: mpsc::UnboundedSender<Msg>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut source = crate::keymap::file::Source::at(path);
        while !stop.load(Ordering::SeqCst) {
            if let Some(reload) = source.poll() {
                let msg = Msg::Keymap {
                    result: reload.result,
                    announce: reload.announce,
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            // Coarser than `INPUT_POLL` because a keymap edit is a human
            // action with human latency tolerance — but slept in `INPUT_POLL`
            // slices, so quitting does not wait out a whole poll interval
            // before this thread can be joined.
            let mut waited = Duration::ZERO;
            while waited < KEYMAP_POLL {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(INPUT_POLL);
                waited += INPUT_POLL;
            }
        }
    })
}

/// Read key presses on their own OS thread until `stop` is set or the channel
/// closes.
fn spawn_input(
    tx: mpsc::UnboundedSender<Msg>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match event::poll(INPUT_POLL) {
                Ok(true) => {}
                // Nothing typed within the window — go round and re-check the
                // stop flag, which is the whole reason for polling with a
                // timeout instead of blocking in `read`.
                Ok(false) => continue,
                // A tty that errors on `poll` does not recover by being asked
                // again, and spinning on it would burn a core. End the thread;
                // the session stays usable (mouse-free, but every RPC result
                // still arrives) and quits on the next Ctrl-C.
                Err(_) => return,
            }
            let Ok(Event::Key(key)) = event::read() else {
                continue;
            };
            // Windows (and some terminals) report press *and* release; only
            // one of them is a keystroke.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let Some(mapped) = to_key(key.code, key.modifiers) else {
                continue;
            };
            if tx.send(Msg::Key(mapped)).is_err() {
                return;
            }
        }
    })
}

/// Translate a crossterm key into the model's vocabulary.
///
/// Unmapped keys return `None` and are dropped here rather than reaching the
/// model, so the model's key handling stays a closed set it can be tested
/// exhaustively against.
fn to_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Char(c) => Some(Key::ctrl(c)),
            _ => None,
        };
    }
    Some(match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognized_theme_name_is_applied() {
        let mut model = Model::new();
        apply_theme_arg(&mut model, Some("mono"));
        assert_eq!(model.theme, theme::Theme::mono());
        assert_eq!(model.level, model::Level::Info, "not treated as an error");
    }

    #[test]
    fn an_unrecognized_theme_name_falls_back_to_dark_and_says_so() {
        let mut model = Model::new();
        apply_theme_arg(&mut model, Some("solarized"));
        assert_eq!(model.theme, theme::Theme::dark());
        assert_eq!(model.level, model::Level::Error);
        assert!(model.status.contains("solarized"), "{}", model.status);

        // The notice this test just set must survive the boot sequence —
        // `Msg::Boot`'s "loading accounts…" would otherwise overwrite it
        // within the first frame, making it unobservable in practice.
        model::update(&mut model, Msg::Boot);
        assert_eq!(model.level, model::Level::Error);
        assert!(model.status.contains("solarized"), "{}", model.status);
    }

    #[test]
    fn no_theme_flag_leaves_the_default_untouched() {
        let mut model = Model::new();
        apply_theme_arg(&mut model, None);
        assert_eq!(model.theme, theme::Theme::default());
        assert_eq!(model.level, model::Level::Info);
    }
}
