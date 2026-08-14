//! `keys.toml`: reading it, watching it, and editing it in place.
//!
//! # A delta, not a replacement
//!
//! The file states what is *different* from the built-in bindings, not the
//! whole keymap. A user who wants `<c-j>` to move down writes one line and
//! keeps everything else; a user who wants `q` gone writes `q = ""`. The
//! alternative — the file being the complete map — means every new binding a
//! release adds is invisible to everyone who ever customised anything, which
//! is how a keymap file becomes a thing people stop upgrading.
//!
//! ```toml
//! [normal]
//! "<c-j>" = "cursor.down"   # bind
//! "d"     = ""              # unbind: `d` now does nothing
//! ```
//!
//! # Hot reload without a file watcher
//!
//! [`Source::poll`] re-reads the file and compares the bytes. Not a
//! dependency on an inotify/FSEvents crate, and not an mtime comparison
//! either: mtime has one-second granularity on filesystems this will actually
//! run on, so an edit saved within the same second as the previous one is
//! invisible to it — precisely the case that matters, because that is what
//! trying two bindings in a row looks like. A `keys.toml` is a few hundred
//! bytes and the poll is once a second, on a thread of its own; reading it is
//! cheaper than being clever about not reading it.
//!
//! A file that stops parsing does **not** clear the user's bindings. The
//! reload reports the error to the status line and the previous keymap keeps
//! working, because the alternative is that a typo mid-edit leaves someone
//! holding a TUI whose keys have all changed.
//!
//! # Editing preserves the rest of the file
//!
//! `mail keys set` rewrites one line and leaves every other byte — comments
//! included — where it was, then re-parses the result and refuses to write
//! anything unless the only binding that changed is the one asked for. See
//! [`edit`].

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{Action, Chord, Keymap, KeymapError, Mode};

/// Environment variable naming the TUI's key-binding file.
pub const KEYS_ENV: &str = "RMAIL_KEYS";

/// The file name, next to the master config.
const KEYS_FILE: &str = "keys.toml";

/// The most `keys.toml` this will read.
///
/// The whole default keymap is under two kilobytes, so this is three orders
/// of magnitude of headroom — and it is here because the path is a *setting*
/// (`$RMAIL_KEYS`), which means a typo can aim it at something that is not a
/// config file at all. Without a bound, `RMAIL_KEYS=/dev/urandom` is a thread
/// allocating forever, once a second, behind a TUI that never says why.
const MAX_KEYS_BYTES: usize = 256 * 1024;

/// Where the key bindings live: `$RMAIL_KEYS`, else `keys.toml` beside the
/// master config file (so `RMAIL_CONFIG` pointing at a scratch directory
/// moves both, which is what a second profile means).
#[must_use]
pub fn keys_path_from_env() -> PathBuf {
    if let Some(path) = std::env::var_os(KEYS_ENV) {
        return PathBuf::from(path);
    }
    let config = crate::config_path_from_env();
    match config.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(KEYS_FILE),
        _ => PathBuf::from(KEYS_FILE),
    }
}

/// One mode's requested changes: `Some(action)` binds, `None` unbinds.
type Section = BTreeMap<Chord, Option<Action>>;

/// The whole file, validated but not yet applied to a keymap.
type Document = BTreeMap<Mode, Section>;

/// The built-in bindings with `text`'s changes applied.
///
/// # Errors
///
/// [`KeymapError`] naming the offending line's mode, chord or action id.
/// `path` appears in parse errors only, so callers that have no file (a
/// `--dry-run`, a test) can pass anything readable.
pub fn parse(text: &str, path: &str) -> Result<Keymap, KeymapError> {
    apply(Keymap::defaults(), &document(text, path)?)
}

/// Read `path`, or the built-in bindings when it does not exist.
///
/// # Errors
///
/// [`KeymapError::Io`] if the file exists but cannot be read, plus everything
/// [`parse`] reports. A missing file is not an error — it is the state every
/// install starts in.
pub fn load(path: &Path) -> Result<Keymap, KeymapError> {
    match read_bounded(path) {
        Ok(text) => parse(&text, &path.display().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Keymap::defaults()),
        Err(source) => Err(KeymapError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The file's text, refusing anything past [`MAX_KEYS_BYTES`].
///
/// `Read::take` rather than a `metadata().len()` check: the point is to be
/// safe on a path that is not a regular file, and those are exactly the ones
/// that report a length of zero and then never end.
fn read_bounded(path: &Path) -> std::io::Result<String> {
    let mut text = String::new();
    let read = std::fs::File::open(path)?
        .take(MAX_KEYS_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    if read > MAX_KEYS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("key bindings must be under {MAX_KEYS_BYTES} bytes"),
        ));
    }
    Ok(text)
}

fn document(text: &str, path: &str) -> Result<Document, KeymapError> {
    let raw: BTreeMap<String, BTreeMap<String, String>> =
        toml::from_str(text).map_err(|source| KeymapError::Toml {
            path: path.to_owned(),
            source,
        })?;

    let mut document = Document::new();
    for (mode_name, bindings) in raw {
        let mode = Mode::from_id(&mode_name).ok_or_else(|| KeymapError::UnknownMode {
            id: mode_name.clone(),
        })?;
        let section = document.entry(mode).or_default();
        for (chord, action) in bindings {
            let chord = Chord::parse(&chord)?;
            let action = if action.is_empty() {
                None
            } else {
                Some(
                    Action::from_id(&action)
                        .ok_or_else(|| KeymapError::UnknownAction { id: action.clone() })?,
                )
            };
            section.insert(chord, action);
        }
    }
    Ok(document)
}

/// Fold a parsed document into `keymap`.
///
/// Unbinds run before binds within a mode: `q = ""` next to `qq = "quit"` is
/// a user trading a one-key binding for a two-key one, and applying them the
/// other way round would reject the pair as a shadow conflict depending on
/// nothing more than how the chords happened to sort.
fn apply(mut keymap: Keymap, document: &Document) -> Result<Keymap, KeymapError> {
    for (mode, section) in document {
        for (chord, action) in section {
            if action.is_none() {
                keymap.unbind(*mode, chord);
            }
        }
        for (chord, action) in section {
            if let Some(action) = action {
                keymap.bind(*mode, chord.clone(), *action)?;
            }
        }
    }
    Ok(keymap)
}

// ---------------------------------------------------------------------------
// hot reload
// ---------------------------------------------------------------------------

/// What one poll of the file found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Snapshot {
    /// No such file — the built-in bindings stand.
    Absent,
    /// The file's bytes.
    Present(String),
    /// It exists and could not be read; the string is why.
    Unreadable(String),
}

/// The bindings to switch to, and whether the user should be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reload {
    /// The new keymap, or why the file was refused. On an error the caller
    /// keeps the keymap it has.
    pub result: Result<Keymap, String>,
    /// Whether this warrants a status line. False for the silent load at
    /// startup, where "keymap reloaded" would stamp on the boot progress the
    /// user is actually waiting for — but never false for an error, which is
    /// worth saying whenever it happens.
    pub announce: bool,
}

/// Watches `keys.toml` for edits. Owns no thread of its own; the caller polls
/// it from wherever it can afford to block on a small file read.
#[derive(Debug)]
pub struct Source {
    path: PathBuf,
    last: Option<Snapshot>,
}

impl Source {
    /// Watch `path`.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    /// Read the file and report a [`Reload`] if anything about it changed
    /// since the previous poll — including its disappearance, which restores
    /// the built-in bindings rather than freezing the ones it used to define.
    pub fn poll(&mut self) -> Option<Reload> {
        let snapshot = match read_bounded(&self.path) {
            Ok(text) => Snapshot::Present(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot::Absent,
            Err(error) => Snapshot::Unreadable(error.to_string()),
        };
        if self.last.as_ref() == Some(&snapshot) {
            return None;
        }
        let first = self.last.is_none();
        self.last = Some(snapshot.clone());

        let path = self.path.display().to_string();
        let result = match &snapshot {
            Snapshot::Absent => Ok(Keymap::defaults()),
            Snapshot::Present(text) => parse(text, &path).map_err(|error| error.to_string()),
            Snapshot::Unreadable(why) => Err(format!("{path}: {why}")),
        };
        match &result {
            Ok(_) => tracing::debug!(path = %path, "key bindings loaded"),
            Err(error) => tracing::warn!(path = %path, %error, "key bindings refused"),
        }
        Some(Reload {
            announce: !first || result.is_err(),
            result,
        })
    }
}

// ---------------------------------------------------------------------------
// editing
// ---------------------------------------------------------------------------

/// `existing` with `chord` bound to `action` in `mode` — or unbound, when
/// `action` is `None` — and every other byte left alone.
///
/// Line-oriented rather than a serialize-the-parsed-document rewrite, because
/// a config file is something a person wrote: comments, ordering and grouping
/// are the parts of it they will miss. The safety net is that the result is
/// re-parsed before it is returned, and anything but the intended change is a
/// refusal ([`KeymapError::EditFailed`]) rather than a write.
///
/// # Errors
///
/// [`KeymapError`] if `existing` does not parse, if the edit would produce a
/// file that does not parse or does not load, or if it would disturb another
/// binding.
pub fn edit(
    existing: &str,
    mode: Mode,
    chord: &Chord,
    action: Option<Action>,
) -> Result<String, KeymapError> {
    // Fail on a file that is already broken before rewriting any of it: the
    // user needs to hear about their own typo, not have it half-fixed.
    let before = document(existing, KEYS_FILE)?;

    let header = format!("[{}]", mode.id());
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut section_end: Option<usize> = None;
    let mut edited = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == header;
        } else if in_section && binds(line, chord) {
            edited = true;
            match action {
                Some(action) => out.push(binding_line(chord, action)),
                // Unbinding drops the line entirely rather than writing
                // `"q" = ""`: the file says what differs from the defaults,
                // and after this the default is what the user wants back.
                None => {
                    section_end = Some(out.len());
                    continue;
                }
            }
            section_end = Some(out.len());
            continue;
        }
        out.push(line.to_owned());
        // Where a new binding would go: after the section's last *non-blank*
        // line. Counting the blank line that separates it from the next
        // section would push the addition below it, which is still legal TOML
        // and still reads as if it belonged to whatever comes next.
        if in_section && !trimmed.is_empty() {
            section_end = Some(out.len());
        }
    }

    if !edited {
        match (action, section_end) {
            (None, _) => {
                // Nothing bound that chord in the file, so nothing to remove.
                // Writing `"q" = ""` instead would be a different request:
                // suppressing a *default* binding, which `set` cannot express
                // and `unset` should not silently start doing.
                return Err(KeymapError::NotBound {
                    chord: chord.to_string(),
                    mode: mode.id(),
                });
            }
            (Some(action), Some(at)) => out.insert(at, binding_line(chord, action)),
            (Some(action), None) => {
                if out.last().is_some_and(|line| !line.trim().is_empty()) {
                    out.push(String::new());
                }
                out.push(header);
                out.push(binding_line(chord, action));
            }
        }
    }

    let mut updated = out.join("\n");
    updated.push('\n');

    // The result has to be a file the TUI would accept, and it has to differ
    // from the original in exactly one binding. Both checks run before the
    // caller is handed anything to write, so a bug in the line surgery above
    // fails loudly instead of quietly eating somebody's bindings.
    let after = document(&updated, KEYS_FILE)?;
    let mut expected = before;
    match action {
        Some(action) => {
            expected
                .entry(mode)
                .or_default()
                .insert(chord.clone(), Some(action));
        }
        None => {
            if let Some(section) = expected.get_mut(&mode) {
                section.remove(chord);
                if section.is_empty() {
                    expected.remove(&mode);
                }
            }
        }
    }
    // An emptied section leaves its `[header]` behind, which parses to an
    // empty map rather than to nothing at all.
    expected.retain(|_, section| !section.is_empty());
    let mut trimmed = after.clone();
    trimmed.retain(|_, section| !section.is_empty());
    if trimmed != expected {
        return Err(KeymapError::EditFailed {
            chord: chord.to_string(),
            mode: mode.id(),
        });
    }
    apply(Keymap::defaults(), &after)?;
    Ok(updated)
}

/// One `"chord" = "action.id"` line. The key is always quoted: TOML's bare
/// keys are `[A-Za-z0-9_-]` only, and half the chords worth binding (`?`,
/// `<c-p>`) are not.
fn binding_line(chord: &Chord, action: Action) -> String {
    format!("{} = \"{}\"", toml_key(&chord.to_string()), action.id())
}

/// Whether `line` is the binding for `chord`, however the user spelled it
/// (`<CR>` and `<enter>` are the same key).
fn binds(line: &str, chord: &Chord) -> bool {
    key_of(line).is_some_and(|key| Chord::parse(&key).is_ok_and(|parsed| &parsed == chord))
}

/// The key half of a `key = value` line, unquoted.
fn key_of(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        // Escapes are not handled, and deliberately: a chord that needs one
        // (a literal quote or backslash) is not a key this TUI can receive,
        // so the only effect of guessing would be to match the wrong line.
        let end = rest.find('"')?;
        let (key, after) = rest.split_at(end);
        after
            .get(1..)?
            .trim_start()
            .starts_with('=')
            .then(|| key.to_owned())
    } else {
        let (key, _) = trimmed.split_once('=')?;
        let key = key.trim();
        (!key.is_empty()).then(|| key.to_owned())
    }
}

/// `value` as a TOML basic string. Only the escapes TOML's grammar requires;
/// see `hook_cli`'s own copy for why this is not a `toml_edit` dependency.
fn toml_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write `contents` to `path` via a process-unique sibling temp file and a
/// rename, so a crash mid-write cannot leave a half-written `keys.toml` —
/// which the TUI would then refuse, leaving the user with a keymap they did
/// not choose.
///
/// Shared by `mail keys set` and `ConfigService.SetBinding` on purpose: two
/// writers of one file must at least agree on *how* they write it, or the
/// crash-safety of whichever one is more careful is worth nothing.
///
/// # Errors
/// Any I/O error creating the parent directory, writing the temp file, or
/// renaming it into place.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // The file's existing permissions, so a rewrite does not silently loosen
    // a mode the user chose.
    let permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let temp: PathBuf = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&temp, contents)?;
    if let Some(permissions) = permissions {
        // Best effort: failing to restore the mode must not abort an
        // otherwise-good write.
        let _ = std::fs::set_permissions(&temp, permissions);
    }
    std::fs::rename(&temp, path)
}
