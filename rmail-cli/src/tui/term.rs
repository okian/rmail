//! Putting the terminal back — on a clean exit, on an error, and on a panic.
//!
//! A TUI takes the terminal out of the state the user's shell left it in: raw
//! mode (no line buffering, no echo, no Ctrl-C signal) and the alternate
//! screen. If the process exits without undoing both, the shell it returns to
//! is unusable — typed characters do not echo, Enter does not submit, Ctrl-C
//! does not interrupt, and the scrollback is gone. The user's recourse is
//! `reset(1)` if they know it exists, or a new terminal window if they do
//! not. This is not a cosmetic failure; it is the worst thing a TUI can do to
//! someone, and it happens precisely when something else has already gone
//! wrong.
//!
//! So restoration is arranged to be unconditional, by two independent
//! mechanisms:
//!
//! 1. **Drop.** [`TerminalGuard`] restores in `Drop`, which covers a normal
//!    return, an early `?` on any error, and stack unwinding.
//! 2. **A panic hook.** Unwinding only runs `Drop` for frames *below* the
//!    panic, and a panic in a thread that is not holding the guard — or a
//!    build with `panic = "abort"`, where nothing unwinds at all — would
//!    never reach it. [`TerminalGuard::enter`] therefore also installs a
//!    panic hook that restores *first* and then calls the hook it replaced,
//!    so the backtrace is printed to a terminal that can display it rather
//!    than being smeared across the alternate screen a moment before that
//!    screen is torn down.
//!
//! Both paths share one [`std::sync::atomic::AtomicBool`], so restoration
//! happens exactly once no matter which fires first (a panic hook that
//! restores, followed by an unwinding `Drop` that restores again, would
//! disable raw mode on a terminal that is no longer in it — harmless today,
//! but it is one `execute!` away from writing escape sequences into the
//! user's shell).
//!
//! # Why the trait
//!
//! The gate runs the test suite in a container with no controlling terminal,
//! where `enable_raw_mode` fails outright. Testing restoration against real
//! crossterm calls is therefore not possible where it matters, so
//! [`TerminalControl`] abstracts the two operations and the tests drive a
//! recording implementation — which is also the only way to assert *ordering*
//! (restore before the previous panic hook) and *exactly once*, neither of
//! which is observable from outside a real terminal.

#[cfg(test)]
mod tests;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// The terminal state a TUI takes over and must give back.
pub trait TerminalControl: Send + Sync + 'static {
    /// Take the terminal over. Failing here must leave nothing to undo.
    ///
    /// # Errors
    ///
    /// Whatever the terminal reports.
    fn enter(&self) -> io::Result<()>;

    /// Give the terminal back. Best effort and infallible by contract: this
    /// runs from `Drop` and from a panic hook, where there is nobody left to
    /// report an error to and panicking again would abort the process.
    fn leave(&self);
}

/// Real crossterm: raw mode plus the alternate screen, on stdout.
///
/// stdout, not stderr, because that is where the ratatui backend writes; the
/// alternate screen has to be entered on the same stream the frames go to.
#[derive(Debug, Clone, Copy)]
pub struct Crossterm;

impl TerminalControl for Crossterm {
    fn enter(&self) -> io::Result<()> {
        enable_raw_mode()?;
        // Ordering matters on the way out as well as in: if entering the
        // alternate screen fails, raw mode is already on and would otherwise
        // leak, so undo it before reporting.
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(())
    }

    fn leave(&self) {
        // All three, unconditionally, ignoring errors: a half-restored
        // terminal is the failure this whole module exists to avoid, so an
        // error leaving the alternate screen must not skip disabling raw
        // mode.
        //
        // `Show` is not redundant. ratatui hides the cursor on every frame
        // that does not position one (`Terminal::flush`), and DECTCEM is a
        // terminal-wide setting, not per-screen-buffer — so leaving the
        // alternate screen without it hands back a shell with an invisible
        // cursor, which is exactly the "unusable terminal" failure this file
        // is about, only subtler.
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Owns the fact that the terminal is in TUI state. Restores on drop.
pub struct TerminalGuard {
    control: Arc<dyn TerminalControl>,
    restored: Arc<AtomicBool>,
}

impl TerminalGuard {
    /// Take the terminal over and arm both restoration paths.
    ///
    /// # Errors
    ///
    /// Whatever [`TerminalControl::enter`] reports. Nothing is installed and
    /// nothing needs undoing when it fails.
    pub fn enter(control: Arc<dyn TerminalControl>) -> io::Result<Self> {
        control.enter()?;
        let restored = Arc::new(AtomicBool::new(false));
        install_panic_hook(Arc::clone(&control), Arc::clone(&restored));
        Ok(Self { control, restored })
    }

    /// Restore now, if nothing has already.
    pub fn restore(&self) {
        restore_once(&self.control, &self.restored);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Restore unless the flag says someone already did.
///
/// `swap` rather than load-then-store: the panic hook can run on a different
/// thread from the one dropping the guard, and two restorations racing is
/// exactly the case this is here to make impossible.
fn restore_once(control: &Arc<dyn TerminalControl>, restored: &Arc<AtomicBool>) {
    if !restored.swap(true, Ordering::SeqCst) {
        control.leave();
    }
}

/// Chain a restore-first step onto whatever panic hook is currently
/// installed, and return the terminal before that hook prints anything.
fn install_panic_hook(control: Arc<dyn TerminalControl>, restored: Arc<AtomicBool>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_once(&control, &restored);
        previous(info);
    }));
}
