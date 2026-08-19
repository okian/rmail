//! The `:` command line's history: an in-memory ring, a file behind it, and
//! the rule about what never reaches either (task 89).
//!
//! # Why there is a file at all
//!
//! A history that dies with the process is one nobody relies on, and the
//! whole value of `<up>` is that yesterday's long invocation is still there.
//! It is a plain newline-delimited file next to `keys.toml`, oldest line
//! first, capped at [`MAX_ENTRIES`] — the same shape and the same directory
//! as the only other piece of TUI state that outlives a session.
//!
//! # What never reaches it
//!
//! [`is_secret`] is consulted before a line is recorded, and it errs toward
//! forgetting. A shell history full of secrets is a well-understood way to
//! leak one, and the recovery — noticing, then editing a file most people do
//! not know exists — is worse than losing a line of convenience.
//!
//! Three rules, all on the *text* rather than on the resolved verb, because
//! the verb registry grows and a line is dangerous the moment it is typed
//! rather than the moment some later release gives it a capability:
//!
//! - a leading `token` verb (`:token create --secret …`),
//! - `account login`, which carries a client id and is followed by a consent
//!   flow,
//! - any `--…secret…`/`--…password…` flag, wherever it appears.
//!
//! # Why the file is `0600`, and how
//!
//! It holds whatever was typed, which is a record of what someone has been
//! reading. [`write`] makes the file `0600` when it is absent *and* when it
//! is not — an existing file that arrived at the umask's default is fixed,
//! rather than written to at whatever mode it happened to have — and then
//! goes through `write_atomic`, which creates its temp file `0600` and copies
//! the destination's mode onto it before the rename. So the content never
//! exists world-readable, on the first write or any later one.
//!
//! # What is filtered on the way in, not only on the way out
//!
//! [`History::new`] applies [`is_secret`] to what it loads. A line written by
//! a build before a rule existed, or added by hand, is otherwise offered by
//! `<up>` *and* carried back out by the next write, because the whole list
//! travels — so a rule that only guarded the write path would leave the file
//! curating itself forever.

#[cfg(test)]
mod tests;

use std::io::Read as _;
use std::path::{Path, PathBuf};

use rmail_core::keymap::file::write_atomic;

/// Environment variable naming the command-line history file.
pub const HISTORY_ENV: &str = "RMAIL_COMMAND_HISTORY";

/// The file name, next to `keys.toml` and the master config.
const HISTORY_FILE: &str = "command-history";

/// The most lines kept, in memory and on disk. The oldest go first.
pub const MAX_ENTRIES: usize = 500;

/// The most history file this will read.
///
/// [`MAX_ENTRIES`] lines of at most `MAX_INPUT` characters is well under a
/// hundred kilobytes; this is two orders of magnitude of headroom, and it is
/// here for the reason `keymap::file`'s own bound is — the path is a setting,
/// so a typo can aim it at something that is not a history file at all.
const MAX_BYTES: usize = 1024 * 1024;

/// Where the history lives: `$RMAIL_COMMAND_HISTORY`, else next to the master
/// config file, so pointing `$RMAIL_CONFIG` at a second profile moves this
/// too — exactly as it moves `keys.toml`.
#[must_use]
pub fn path_from_env() -> PathBuf {
    if let Some(path) = std::env::var_os(HISTORY_ENV) {
        return PathBuf::from(path);
    }
    let config = rmail_core::config_path_from_env();
    match config.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(HISTORY_FILE),
        _ => PathBuf::from(HISTORY_FILE),
    }
}

/// The recorded lines, oldest first.
///
/// A file that cannot be read is an empty history rather than an error: this
/// is a convenience, and a TUI that refuses to start because a history file
/// is unreadable has traded something valuable for something that is not.
#[must_use]
pub fn read(path: &Path) -> Vec<String> {
    let Ok(text) = read_bounded(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    // A file that grew past the cap — by hand, or by an older build — is
    // trimmed on read rather than trusted, so the in-memory ring is bounded
    // by this constant and not by what is on disk.
    if lines.len() > MAX_ENTRIES {
        lines.drain(..lines.len() - MAX_ENTRIES);
    }
    lines
}

fn read_bounded(path: &Path) -> std::io::Result<String> {
    // Checked *before* the open, not bounded after it: `take` bounds how much
    // is read, and `open(2)` on a FIFO with no writer blocks before there is
    // anything to bound. The path is a setting, so a typo can aim it at one —
    // and this read happens at startup with the terminal already in raw mode,
    // where a hang is a wedged session rather than an error message.
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    // `take` rather than a length check: the point is to be safe on a path
    // that is not a regular file, and those report a length of zero and then
    // never end.
    file.take(MAX_BYTES as u64).read_to_string(&mut text)?;
    Ok(text)
}

/// Write `entries`, oldest first, at `0600`.
///
/// # Errors
///
/// Whatever creating or writing the file reports. Callers treat this as
/// advisory — see the module docs.
pub fn write(path: &Path, entries: &[String]) -> std::io::Result<()> {
    ensure_private(path)?;
    let mut text = String::new();
    for entry in entries.iter().rev().take(MAX_ENTRIES).rev() {
        text.push_str(entry);
        text.push('\n');
    }
    write_atomic(path, &text)
}

/// Create `path` at `0600` if it does not exist, so [`write_atomic`]'s
/// "preserve the destination's mode" has a private mode to preserve.
fn ensure_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt as _;

        // Fixed rather than accepted: a file that arrived at the umask's
        // default — from an older build, a restore, a copy — would otherwise
        // stay world-readable for ever, because `write_atomic` preserves
        // whatever mode it finds.
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        // Somebody else created it between the check and the open. Their
        // mode, not ours — the same answer `write_atomic` gives for a file
        // that already exists.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether this line must never be recorded.
///
/// See the module docs for the three rules and why they read the text rather
/// than the resolved verb.
#[must_use]
pub fn is_secret(line: &str) -> bool {
    // Quotes are stripped before anything is read, because `command::tokenize`
    // strips them too: `account login "--password" x` is the same invocation
    // as the unquoted one, and a rule that could be evaded by quoting is a
    // rule with a published bypass.
    let bare: String = strip_prefixes(line).replace(['"', '\''], "");
    let words: Vec<&str> = bare.split_whitespace().collect();
    if words
        .iter()
        .any(|word| flag_name(word).is_some_and(sensitive_flag))
    {
        return true;
    }
    // Dots and spaces are the same separator everywhere in this vocabulary,
    // so `:account.login` is the same line as `:account login` and must be
    // forgotten by the same rule.
    // Lowercased, because the fallback dispatch matches a verb
    // case-insensitively (`overlays::verb_words`) — so `:TOKEN create` will
    // run the `token` verb the day one exists, and must be forgotten by the
    // same rule that forgets the lowercase spelling.
    let path: Vec<String> = words
        .iter()
        .take_while(|word| !word.starts_with('-'))
        .flat_map(|word| word.split('.'))
        .filter(|segment| !segment.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    matches!(path.as_slice(), ["token", ..] | ["account", "login", ..])
}

/// A line with its leading `:`, range and trailing `!` removed — what the
/// rules above are about.
fn strip_prefixes(line: &str) -> &str {
    let rest = line.trim().trim_start_matches(':').trim_start();
    let rest = rest
        .strip_prefix("'<,'>")
        .or_else(|| rest.strip_prefix('%'))
        .unwrap_or(rest);
    rest.trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start()
}

/// `--secret-env=x` → `secret-env`. `None` for anything that is not a long
/// flag.
fn flag_name(word: &str) -> Option<&str> {
    let name = word.strip_prefix("--")?;
    Some(name.split('=').next().unwrap_or(name))
}

fn sensitive_flag(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("secret") || lower.contains("password")
}

/// The recorded lines, oldest first, and the browse this pane is doing
/// through them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    entries: Vec<String>,
}

impl History {
    /// A history holding `entries`, oldest first, trimmed to the cap and with
    /// anything [`is_secret`] refuses dropped.
    ///
    /// Filtered here rather than only in [`History::record`] — see the module
    /// docs: the whole list is what gets written back, so a line the rule
    /// would refuse today must not survive in the file because an older build
    /// let it in.
    #[must_use]
    pub fn new(entries: Vec<String>) -> Self {
        let mut entries: Vec<String> = entries
            .into_iter()
            .filter(|line| !line.trim().is_empty() && !is_secret(line))
            .collect();
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
        }
        Self { entries }
    }

    /// Every line, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record `line`, unless [`is_secret`] refuses it or it is blank.
    ///
    /// A line identical to the most recent one moves rather than repeats: a
    /// command run three times in a row should cost one `<up>`, not three.
    /// Returns whether anything was recorded, which is what tells the caller
    /// there is a file to rewrite.
    pub fn record(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() || is_secret(line) {
            return false;
        }
        if let Some(at) = self.entries.iter().position(|entry| entry == line) {
            self.entries.remove(at);
        }
        self.entries.push(line.to_owned());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
        }
        true
    }

    /// The recorded lines starting with `prefix`, newest first.
    ///
    /// An empty prefix matches everything, which is what makes a bare `<up>`
    /// on an empty line walk the whole history.
    #[must_use]
    pub fn matching(&self, prefix: &str) -> Vec<&str> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.starts_with(prefix))
            .map(String::as_str)
            .collect()
    }
}
