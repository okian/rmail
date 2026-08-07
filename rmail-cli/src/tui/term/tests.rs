//! Tests for terminal restoration.
//!
//! nextest runs every test in its own process, which is what makes it safe to
//! install a process-wide panic hook here: no other test can observe it.

use std::sync::Mutex;

use super::*;

/// Records every state transition, in order.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<&'static str>>,
    fail_enter: bool,
}

impl Recorder {
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

impl TerminalControl for Recorder {
    fn enter(&self) -> io::Result<()> {
        if self.fail_enter {
            return Err(io::Error::other("no tty"));
        }
        self.events.lock().unwrap().push("enter");
        Ok(())
    }

    fn leave(&self) {
        self.events.lock().unwrap().push("leave");
    }
}

fn recorder() -> Arc<Recorder> {
    Arc::new(Recorder::default())
}

#[test]
fn dropping_the_guard_restores_the_terminal() {
    let control = recorder();
    {
        let _guard =
            TerminalGuard::enter(Arc::clone(&control) as Arc<dyn TerminalControl>).unwrap();
        assert_eq!(control.events(), vec!["enter"]);
    }
    assert_eq!(control.events(), vec!["enter", "leave"]);
}

#[test]
fn an_early_error_path_still_restores() {
    // The shape every `?` in `tui::run` after `TerminalGuard::enter` relies
    // on: the guard is a local, so returning an error unwinds through its
    // `Drop`.
    let control = recorder();
    fn fallible(control: Arc<dyn TerminalControl>) -> anyhow::Result<()> {
        let _guard = TerminalGuard::enter(control)?;
        anyhow::bail!("the terminal is too small, or whatever else went wrong");
    }
    assert!(fallible(Arc::clone(&control) as Arc<dyn TerminalControl>).is_err());
    assert_eq!(control.events(), vec!["enter", "leave"]);
}

#[test]
fn restoring_twice_only_leaves_once() {
    // The panic hook and `Drop` both restore, and on a panic both run. A
    // second `leave` on a terminal already back in cooked mode is at best
    // wasted escape sequences written into the user's shell.
    let control = recorder();
    {
        let guard = TerminalGuard::enter(Arc::clone(&control) as Arc<dyn TerminalControl>).unwrap();
        guard.restore();
        guard.restore();
    }
    assert_eq!(control.events(), vec!["enter", "leave"]);
}

#[test]
fn a_panic_restores_the_terminal_before_the_previous_hook_prints() {
    let control = recorder();

    // Stand in for the default hook (which would print the panic message and
    // the backtrace) so the ordering between the two is observable.
    let marker = Arc::clone(&control);
    std::panic::set_hook(Box::new(move |_| {
        marker.events.lock().unwrap().push("previous-hook");
    }));

    let guard = TerminalGuard::enter(Arc::clone(&control) as Arc<dyn TerminalControl>).unwrap();
    let result = std::panic::catch_unwind(|| {
        // Stands in for a bug anywhere in the TUI. `unreachable!` rather than
        // `panic!`/`panic_any`, which the workspace lints deny even here; the
        // unwinding behaviour under test is identical.
        unreachable!("a bug somewhere in the TUI");
    });
    assert!(result.is_err(), "the closure really did panic");
    drop(guard);

    assert_eq!(
        control.events(),
        vec!["enter", "leave", "previous-hook"],
        "the terminal is back before anything is printed, and the unwinding \
         Drop did not leave a second time"
    );
}

#[test]
fn a_failed_enter_leaves_nothing_to_undo() {
    let control = Arc::new(Recorder {
        events: Mutex::new(Vec::new()),
        fail_enter: true,
    });
    let outcome = TerminalGuard::enter(Arc::clone(&control) as Arc<dyn TerminalControl>)
        .map(|_| ())
        .map_err(|error| error.to_string());
    assert_eq!(outcome, Err("no tty".to_owned()));
    assert!(
        control.events().is_empty(),
        "nothing was entered, so nothing may be left"
    );
}
