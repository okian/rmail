//! `mail keys list/set/unset/actions` — the TUI's key bindings from the
//! command line.
//!
//! # These edit a file; they are not RPCs
//!
//! Key bindings are a property of the terminal in front of the person
//! pressing the keys, not of the daemon. `keys.toml` therefore lives beside
//! the master config (`$RMAIL_KEYS`, else `keys.toml` next to `$RMAIL_CONFIG`)
//! and these verbs read and rewrite it directly — the same shape `mail hook
//! add` has, and for the same reason: a second writer reached through a
//! daemon would have to be kept in sync with the file a user edits by hand,
//! and would still be editing that same file at the end of it.
//!
//! Nothing needs restarting afterwards. A running `mail tui` re-reads the
//! file within a second and swaps its bindings live (`keymap::file::Source`),
//! so `mail keys set` in one pane takes effect in the TUI in another.
//!
//! The write is crash-safe (write a process-unique sibling temp file, then
//! rename) but not safe against two concurrent `mail keys set` invocations:
//! both read the same "before" and the second rename wins. That is the same
//! read-modify-write race every lock-free config editor has, and the same one
//! `hook_cli` documents.

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::keymap::file::{self, keys_path_from_env};
use crate::keymap::{Action, Chord, Keymap, Mode};

/// `mail keys <action>`.
#[derive(Debug, Subcommand)]
pub enum KeysAction {
    /// Show the bindings in force, mode by mode.
    List {
        /// Only this mode (normal, viewer, visual, insert, pick, confirm,
        /// help).
        #[arg(long)]
        mode: Option<String>,
    },
    /// Bind a chord to an action, e.g. `mail keys set '<c-j>' cursor.down`.
    Set {
        /// The chord, in vim notation: `gg`, `<c-p>`, `<esc>` … Quote it —
        /// `<` and `>` are shell redirections.
        chord: String,
        /// The action id. `mail keys actions` lists every one.
        action: String,
        /// Which mode to bind in.
        #[arg(long, default_value = "normal")]
        mode: String,
    },
    /// Remove a binding this file added, restoring the built-in one.
    Unset {
        /// The chord, in vim notation.
        chord: String,
        /// Which mode to unbind in.
        #[arg(long, default_value = "normal")]
        mode: String,
    },
    /// List every action id a binding can name.
    Actions,
}

/// Run one `mail keys` verb.
///
/// # Errors
///
/// If `keys.toml` cannot be read, does not parse, names an unknown mode,
/// chord or action, or cannot be written back.
pub fn run(action: KeysAction) -> Result<()> {
    let path = keys_path_from_env();
    match action {
        KeysAction::List { mode } => list(&path, mode.as_deref()),
        KeysAction::Set {
            chord,
            action,
            mode,
        } => set(&path, &mode, &chord, Some(&action)),
        KeysAction::Unset { chord, mode } => set(&path, &mode, &chord, None),
        KeysAction::Actions => {
            for action in Action::ALL {
                println!("{:<22}{}", action.id(), action.describe());
            }
            Ok(())
        }
    }
}

fn parse_mode(id: &str) -> Result<Mode> {
    Mode::from_id(id).ok_or_else(|| {
        anyhow::Error::new(crate::keymap::KeymapError::UnknownMode { id: id.to_owned() })
    })
}

fn list(path: &Path, only: Option<&str>) -> Result<()> {
    let wanted = only.map(parse_mode).transpose()?;
    let keymap = file::load(path)
        .with_context(|| format!("reading key bindings from {}", path.display()))?;

    println!("# {}", path.display());
    for mode in Mode::CONFIGURABLE {
        if wanted.is_some_and(|only| only != *mode) {
            continue;
        }
        println!("\n[{}]", mode.id());
        // The effective bindings, not the layer's own: a user reading this to
        // find out what `j` does in the viewer should not have to know that
        // the viewer inherits it from normal mode.
        let mut printed = 0_usize;
        for action in Action::ALL {
            for chord in keymap.chords_for(*mode, *action) {
                println!(
                    "{:<10}{:<22}{}",
                    chord.to_string(),
                    action.id(),
                    action.describe()
                );
                printed += 1;
            }
        }
        if printed == 0 {
            println!("(nothing bound)");
        }
    }
    Ok(())
}

/// Bind or unbind one chord, rewriting only the line it is on.
fn set(path: &Path, mode: &str, chord: &str, action: Option<&str>) -> Result<()> {
    let mode = parse_mode(mode)?;
    let chord = Chord::parse(chord)?;
    let action = action
        .map(|id| {
            Action::from_id(id).ok_or_else(|| {
                anyhow::Error::new(crate::keymap::KeymapError::UnknownAction { id: id.to_owned() })
            })
        })
        .transpose()?;

    // A missing file is the normal first-run state and becomes an empty base
    // to add to. Any *other* read error must not be treated the same way: the
    // write below would then replace a real (unread) file with one holding
    // nothing but this binding.
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading key bindings from {}", path.display()))
        }
    };

    let updated = file::edit(&existing, mode, &chord, action)?;
    // What the TUI would end up with. Checked here so a binding that cannot
    // work — a chord that shadows another, an insert-mode chord — is refused
    // before it is written rather than reported into a status line later.
    let keymap: Keymap = file::parse(&updated, &path.display().to_string())?;
    write_atomically(path, &updated)?;

    match action {
        Some(action) => println!(
            "bound {chord} to {} in {} mode ({})",
            action.id(),
            mode.id(),
            path.display()
        ),
        None => println!(
            "unbound {chord} in {} mode ({}) — {}",
            mode.id(),
            path.display(),
            match keymap.lookup(mode, &chord) {
                Some(action) => format!("it is {} again", action.id()),
                None => "it now does nothing".to_owned(),
            }
        ),
    }
    println!("a running `mail tui` picks this up within a second; nothing to restart");
    Ok(())
}

/// Write `keys.toml` crash-safely.
///
/// Delegates to [`rmail_core::keymap::file::write_atomic`], which
/// `ConfigService.SetBinding` also calls: one file, one way of writing it.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    rmail_core::keymap::file::write_atomic(path, contents)
        .with_context(|| format!("writing {}", path.display()))
}
