//! "Open HTML in browser": write the message's HTML alternative to a private
//! temp file and hand that file to the platform opener.
//!
//! rmail does not render HTML mail in the terminal (prd.md: "No HTML
//! rendering initially; 'Open HTML in browser'"). Handing it to a real
//! browser is the whole feature, and it has two ways to go wrong that are
//! worth spelling out, because both are silent.
//!
//! # The file is private, and stays private
//!
//! Mail is private. `std::fs::File::create` uses mode `0666 & !umask`, and a
//! umask of `022` — the default on macOS and most Linux distributions — makes
//! that `0644`: every other account on the machine can read the message.
//! [`write_private`] creates the file with an explicit `0o600` *at open time*
//! via [`std::os::unix::fs::OpenOptionsExt::mode`], not by chmod-ing
//! afterwards, because a chmod after the fact leaves a window in which the
//! file exists, contains the mail, and is world-readable. `create_new` also
//! makes the open fail rather than follow a pre-existing symlink an attacker
//! planted at the predictable path — the classic temp-file race, which on a
//! shared `/tmp` would otherwise let them redirect the write.
//!
//! # The path is an argument, never a fragment of a shell command
//!
//! The opener is spawned as a program plus one argv entry
//! ([`std::process::Command::new`] / `.arg`), never as a string handed to a
//! shell. This is the same invariant `rmail_core::hooks` documents at length
//! for hook commands, and it matters here for the same reason: a path that
//! reaches a shell is a path whose metacharacters are *syntax*. The path this
//! module generates is built from a pid and a counter and contains none —
//! but "the value is safe today" is not the property worth relying on when
//! the alternative is a construction where injection is unrepresentable.
//! `tests::opener_passes_the_path_as_one_argv_entry_never_through_a_shell`
//! spawns the real opener on a path full of metacharacters and asserts both
//! that the child received it verbatim as `$1` and that nothing the path
//! "says" executed.
//!
//! # The document is wrapped in a deny-all CSP
//!
//! HTML mail is attacker-controlled markup, and opening it from `file://`
//! gives it a browser. [`wrap`] therefore does not write the message's HTML
//! out as-is: it embeds it in a minimal document whose first element is a
//! `Content-Security-Policy` meta tag denying every fetch except inline
//! styles and `data:` images. That blocks remote script, remote CSS, frames,
//! form posts — and, most routinely, the remote-image tracking pixel that
//! tells a sender exactly when their mail was read. Blocking those is the
//! behaviour a local-first mail client owes its user; a plain dump of the
//! part would quietly do the opposite.
//!
//! # The file is deleted when the TUI exits, not when the opener returns
//!
//! The opener returns as soon as the browser is *launched*; the browser reads
//! the file some unpredictable time later. Deleting on return would race it
//! into a blank tab. But leaving decoded mail in `/tmp` until the machine
//! reboots is a real privacy cost for a local-first mail client, so
//! [`sweep`] removes this process's own files on the way out — by which
//! point every browser this session launched has long since read them.

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};

/// Mode for the temp file: owner read/write, nothing for anyone else.
const PRIVATE_MODE: u32 = 0o600;

/// Launches a file in whatever the platform considers its default handler.
pub trait Opener: Send + Sync {
    /// Open `path`.
    ///
    /// # Errors
    ///
    /// If the opener cannot be spawned or exits non-zero.
    fn open(&self, path: &Path) -> Result<()>;
}

/// The platform opener: `open` on macOS, `xdg-open` elsewhere.
#[derive(Debug, Clone)]
pub struct CommandOpener {
    program: String,
}

impl CommandOpener {
    /// The opener for the platform this was built for.
    #[must_use]
    pub fn platform() -> Self {
        Self::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        })
    }

    /// An opener that runs `program`. Exposed so the tests can substitute a
    /// program that records its arguments.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Opener for CommandOpener {
    fn open(&self, path: &Path) -> Result<()> {
        // One program, one argument. No shell is involved anywhere on this
        // path, so there is no string for the path to be syntax within.
        //
        // All three streams are `/dev/null`. The child inherits this
        // process's, and this process's stdout *is the alternate screen*:
        // `xdg-open`'s diagnostics ("no method available for opening …"), or
        // any chatter from the handler it picks, would be written straight
        // into cells ratatui does not know it wrote — and because ratatui
        // diffs against its own previous buffer, it would never repaint them.
        // The garbage would sit there for the rest of the session.
        let mut child = Command::new(&self.program)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("launching `{}`", self.program))?;

        // Launched, not finished. Waiting for the handler to exit would hold
        // this blocking task open for as long as the user's browser is
        // open — and since a runtime shutdown waits for its blocking pool,
        // `mail tui` would refuse to quit until they closed it. A detached
        // reaper thread collects the child whenever it does exit, so nothing
        // is left a zombie either. The cost is that a handler which fails
        // *after* spawning cannot be reported; the status line names the file
        // instead, so it can still be opened by hand.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Write `html` somewhere private and open it. Returns the file written.
///
/// Blocking: file creation and spawning a child process both are. Callers run
/// this on a blocking task, never on the event loop (see [`crate::tui::grpc`]).
///
/// # Errors
///
/// If the file cannot be created or the opener cannot be run.
pub fn open_in_browser(message_id: i64, html: &str, opener: &dyn Opener) -> Result<PathBuf> {
    let path = temp_path(message_id);
    write_private(&path, wrap(html).as_bytes())?;
    opener.open(&path)?;
    Ok(path)
}

/// Create `path` with mode `0600` and write `contents` to it.
///
/// # Errors
///
/// If the file already exists, is a symlink, or cannot be written.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_MODE)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flushing {}", path.display()))
}

/// Embed a message's HTML in a document that cannot fetch anything.
///
/// The CSP is a `<meta>` tag rather than a header because there is no server
/// here — a `file://` document's only way to declare a policy is in its own
/// `<head>`, and it must come before any content that could trigger a fetch.
#[must_use]
pub fn wrap(html: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html><head>\n\
         <meta charset=\"utf-8\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">\n\
         <title>rmail message</title>\n\
         </head><body>\n{html}\n</body></html>\n"
    )
}

/// The filename prefix every file this module writes shares, ending in this
/// process's pid so [`sweep`] can only ever delete its own.
fn prefix() -> String {
    format!("rmail-msg-{}-", std::process::id())
}

/// A fresh path in the user's temp directory for one message.
///
/// pid plus a monotonic counter, matching `note_cli::temp_note_path`'s
/// reasoning: there is no `tempfile` dependency in this workspace, and this
/// is enough to avoid colliding with another `mail tui` in another terminal.
/// Collision is not the security boundary in any case — [`write_private`]'s
/// `create_new` is, and it fails closed.
fn temp_path(message_id: i64) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{}{n}-{message_id}.html", prefix()))
}

/// Delete every HTML file *this process* wrote. Best effort: a file the
/// browser still has open, or one already gone, is not worth a message on the
/// way out.
///
/// Scoped by pid, not by the `rmail-msg-` prefix alone, so a concurrent
/// `mail tui` in another terminal never has its open message deleted out from
/// under it.
pub fn sweep() {
    let prefix = prefix();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
