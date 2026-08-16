//! The consumer half of an export: applying a [`Chunk`] stream to a
//! filesystem destination.
//!
//! # Why this lives in the library and not in the CLI
//!
//! The `mail` CLI is one client of `ExportService.Export`; the TUI and any
//! MCP-driven caller are others, and every one of them has to reassemble the
//! same stream into the same archive. A second implementation of "append
//! these bytes to that file" is a second place for the Maildir layout, the
//! chunk-ordering contract, and — most importantly — the path check to be
//! subtly different.
//!
//! # `path` comes off a socket, so it is validated, not trusted
//!
//! The server generates every [`Chunk::path`] from a message id and a
//! sanitized subject slug, so in this codebase they are safe by construction.
//! That is not the property this module relies on. A client hands a remote
//! peer's string straight to `join()` at its peril: an absolute path replaces
//! the destination entirely (`Path::join("/etc/cron.d/x")` *is*
//! `/etc/cron.d/x`), and `..` walks out of it. [`safe_join`] rejects both,
//! along with every non-`Normal` component, so a compromised or simply buggy
//! daemon cannot write outside the directory the user named.
//!
//! What that check does **not** cover, and cannot from here: a pre-existing
//! symlink *inside* the destination directory, planted by a local attacker
//! before the export ran, is still followed by `File::create`. Defending that
//! needs `O_NOFOLLOW`-per-component resolution and is a property of the
//! destination the user chose, not of the bytes the server sent. It is called
//! out here rather than left to be assumed away.
//!
//! # Blocking, on purpose
//!
//! Every call here is synchronous `std::fs`. A caller on an async runtime
//! drives it from a single [`tokio::task::spawn_blocking`] fed by a channel
//! (see `rmail-cli`'s `export_cli`), which is both cheaper and clearer than
//! wrapping each individual write.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};

use super::{Chunk, Format};

/// The subdirectories every Maildir must have.
///
/// `cur/` is where an export's messages land (see [`super::maildir`]);
/// `new/` and `tmp/` are created empty because a directory missing either is
/// not a Maildir and the next tool to deliver into it will say so.
pub const MAILDIR_DIRS: [&str; 3] = ["tmp", "new", "cur"];

/// Why applying an export stream to a destination failed.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The destination could not be created, written, or flushed.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory involved.
        path: String,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
    /// A chunk's `path` is not a safe relative path inside the destination.
    #[error("refusing to write export entry {path:?}: {reason}")]
    UnsafePath {
        /// The rejected path, as received.
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A chunk arrived that the stream contract says cannot: a continuation
    /// for a file that is not open, a named entry in a single-stream format,
    /// or an unnamed one in a per-file format.
    #[error("malformed export stream: {0}")]
    Protocol(String),
}

impl WriteError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// Where an export's bytes go.
enum Sink {
    /// One document, on disk: mbox or JSON. Kept distinct from
    /// [`Sink::Stream`] so [`DestinationWriter::finish`] can `fsync` it —
    /// flushing a `BufWriter` only moves bytes into the page cache, and an
    /// archive that reported success and then lost its tail to a power cut is
    /// exactly the failure this whole module exists to prevent.
    File(BufWriter<File>),
    /// One document, into something that is not a file: stdout, a pipe, a
    /// buffer. There is nothing to sync.
    Stream(Box<dyn Write + Send>),
    /// One file per message, rooted at a directory.
    PerFile {
        root: PathBuf,
        /// The entry currently being appended to, if any.
        open: Option<(String, BufWriter<File>)>,
    },
}

impl Sink {
    /// The single-document writer, whichever kind it is.
    fn writer(&mut self) -> Option<&mut dyn Write> {
        match self {
            Sink::File(file) => Some(file),
            Sink::Stream(stream) => Some(stream),
            Sink::PerFile { .. } => None,
        }
    }
}

/// Applies a chunk stream to a destination.
///
/// Create it, feed it every chunk in the order they arrive, then
/// [`finish`](DestinationWriter::finish) — which is what flushes, and
/// therefore the only place a late I/O error can surface.
pub struct DestinationWriter {
    format: Format,
    sink: Sink,
}

/// Hand-written because [`Sink::Stream`] holds a `dyn Write`, which has no
/// `Debug`. What a reader wants from this type is where it is writing, not
/// the writer's own guts.
impl std::fmt::Debug for DestinationWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("DestinationWriter");
        out.field("format", &self.format);
        match &self.sink {
            Sink::File(_) => out.field("destination", &"<file>"),
            Sink::Stream(_) => out.field("destination", &"<stream>"),
            Sink::PerFile { root, open } => out
                .field("root", root)
                .field("open", &open.as_ref().map(|(path, _)| path)),
        };
        out.finish()
    }
}

impl DestinationWriter {
    /// Create a writer for `format` at `destination`.
    ///
    /// For a single-stream format `destination` is a file, created or
    /// truncated. For a per-file format it is a directory, created if
    /// missing — plus `tmp/`, `new/` and `cur/` for a Maildir. An existing
    /// directory is reused rather than refused: Maildir names are unique per
    /// message, so re-exporting overwrites each message's own file instead of
    /// accumulating duplicates.
    ///
    /// # Errors
    ///
    /// [`WriteError::Io`] if the destination cannot be created.
    pub fn create(format: Format, destination: &Path) -> Result<Self, WriteError> {
        let sink = if format.is_single_stream() {
            if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| WriteError::io(parent, e))?;
            }
            let file = File::create(destination).map_err(|e| WriteError::io(destination, e))?;
            Sink::File(BufWriter::new(file))
        } else {
            std::fs::create_dir_all(destination).map_err(|e| WriteError::io(destination, e))?;
            if format == Format::Maildir {
                for dir in MAILDIR_DIRS {
                    let path = destination.join(dir);
                    std::fs::create_dir_all(&path).map_err(|e| WriteError::io(&path, e))?;
                }
            }
            Sink::PerFile {
                root: destination.to_path_buf(),
                open: None,
            }
        };
        Ok(Self { format, sink })
    }

    /// Create a writer that streams a single-stream format into `writer` —
    /// stdout, a pipe, a buffer.
    ///
    /// # Errors
    ///
    /// [`WriteError::Protocol`] for a per-file format, which needs somewhere
    /// to put many files and cannot be squeezed through one stream.
    pub fn to_writer(format: Format, writer: Box<dyn Write + Send>) -> Result<Self, WriteError> {
        if !format.is_single_stream() {
            return Err(WriteError::Protocol(format!(
                "{format} writes one file per message and needs a directory, not a stream"
            )));
        }
        Ok(Self {
            format,
            sink: Sink::Stream(writer),
        })
    }

    /// Apply one chunk.
    ///
    /// # Errors
    ///
    /// [`WriteError::UnsafePath`] for a path that escapes the destination,
    /// [`WriteError::Protocol`] for a chunk the stream contract forbids, and
    /// [`WriteError::Io`] for a filesystem failure.
    pub fn apply(&mut self, chunk: &Chunk) -> Result<(), WriteError> {
        let format = self.format;
        match &mut self.sink {
            Sink::PerFile { root, open } => {
                let Some(path) = chunk.path.as_deref() else {
                    return Err(WriteError::Protocol(format!(
                        "{format} writes one file per message, but a chunk named no entry"
                    )));
                };
                if chunk.start_of_message {
                    // Flush and drop the previous entry before opening the
                    // next, so an I/O error on close is reported against the
                    // file that caused it rather than swallowed.
                    close_entry(open.take())?;
                    let full = safe_join(root, path)?;
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| WriteError::io(parent, e))?;
                    }
                    let file = File::create(&full).map_err(|e| WriteError::io(&full, e))?;
                    *open = Some((path.to_owned(), BufWriter::new(file)));
                }
                match open {
                    Some((current, writer)) if current == path => {
                        writer.write_all(&chunk.data).map_err(|e| WriteError::Io {
                            path: path.to_owned(),
                            source: e,
                        })
                    }
                    _ => Err(WriteError::Protocol(format!(
                        "a continuation chunk arrived for {path:?}, which is not the open entry"
                    ))),
                }
            }
            single => {
                if let Some(path) = &chunk.path {
                    return Err(WriteError::Protocol(format!(
                        "{format} is a single document, but a chunk named the entry {path:?}"
                    )));
                }
                let Some(writer) = single.writer() else {
                    return Err(WriteError::Protocol(
                        "internal: a single-document sink with no writer".to_owned(),
                    ));
                };
                writer.write_all(&chunk.data).map_err(|e| WriteError::Io {
                    path: "<export stream>".to_owned(),
                    source: e,
                })
            }
        }
    }

    /// Flush, sync, and close.
    ///
    /// Every file this writer opened is `fsync`ed here. Flushing a
    /// `BufWriter` only moves bytes into the kernel's page cache, so without
    /// this an export could report success and then lose its tail to a power
    /// cut — a silently short archive, which is the failure mode this module
    /// exists to make impossible. That also makes `finish` the place a full
    /// disk finally shows up, and why it is not a `Drop` impl that could only
    /// swallow the error.
    ///
    /// # Errors
    ///
    /// [`WriteError::Io`] if the flush or the sync fails.
    pub fn finish(self) -> Result<(), WriteError> {
        match self.sink {
            Sink::File(file) => sync_file(file, "<export file>"),
            Sink::Stream(mut writer) => writer.flush().map_err(|e| WriteError::Io {
                path: "<export stream>".to_owned(),
                source: e,
            }),
            Sink::PerFile { open, .. } => close_entry(open),
        }
    }
}

/// Flush, sync and drop an open per-file entry.
fn close_entry(entry: Option<(String, BufWriter<File>)>) -> Result<(), WriteError> {
    let Some((path, writer)) = entry else {
        return Ok(());
    };
    sync_file(writer, &path)
}

/// Flush a buffered file all the way to the disk.
fn sync_file(writer: BufWriter<File>, path: &str) -> Result<(), WriteError> {
    // `into_inner` flushes first; its error type carries both the underlying
    // `io::Error` and (uselessly here) the writer itself.
    let file = writer.into_inner().map_err(|e| WriteError::Io {
        path: path.to_owned(),
        source: e.into_error(),
    })?;
    file.sync_all().map_err(|e| WriteError::Io {
        path: path.to_owned(),
        source: e,
    })
}

/// Join a server-supplied relative path onto a destination root, refusing
/// anything that could land outside it.
///
/// # Errors
///
/// [`WriteError::UnsafePath`] for an empty path, an absolute one, a
/// Windows-style prefix, or one with a `..` (or leading `.`) component. An
/// interior `.` never reaches the loop — `Path::components` folds it away —
/// and the result stays inside the root either way.
pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, WriteError> {
    let unsafe_path = |reason: &'static str| WriteError::UnsafePath {
        path: relative.to_owned(),
        reason,
    };
    if relative.is_empty() {
        return Err(unsafe_path("the entry path is empty"));
    }
    // A NUL cannot reach a syscall as part of a path, and rejecting it here
    // gives a comprehensible error instead of an `InvalidInput` from `open`.
    if relative.contains('\0') {
        return Err(unsafe_path("the entry path contains a NUL byte"));
    }
    let candidate = Path::new(relative);
    let mut out = root.to_path_buf();
    let mut components = 0;
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                components += 1;
            }
            Component::CurDir => return Err(unsafe_path("the entry path contains `.`")),
            Component::ParentDir => return Err(unsafe_path("the entry path contains `..`")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path("the entry path is absolute"))
            }
        }
    }
    if components == 0 {
        return Err(unsafe_path("the entry path names no file"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_entry_path_is_refused() {
        let error = safe_join(Path::new("/tmp/dest"), "/etc/passwd").unwrap_err();
        assert!(matches!(error, WriteError::UnsafePath { .. }), "{error}");
    }

    #[test]
    fn a_traversing_entry_path_is_refused() {
        for path in [
            "../escape.eml",
            "cur/../../escape.eml",
            "./escape.eml",
            "//etc/passwd",
        ] {
            let error = safe_join(Path::new("/tmp/dest"), path).unwrap_err();
            assert!(
                matches!(error, WriteError::UnsafePath { .. }),
                "{path} was accepted"
            );
        }
    }

    /// An interior `.` never reaches this check — `Path::components` folds it
    /// away — and the result is still inside the root, which is the property
    /// that matters. Pinned so the normalization is a decision rather than a
    /// surprise if this check is ever rewritten over raw string splitting.
    #[test]
    fn an_interior_current_dir_component_normalizes_inside_the_root() {
        let joined = safe_join(Path::new("/tmp/dest"), "a/./b.eml").unwrap();
        assert_eq!(joined, Path::new("/tmp/dest/a/b.eml"));
    }

    #[test]
    fn an_empty_or_nul_entry_path_is_refused() {
        assert!(safe_join(Path::new("/tmp/dest"), "").is_err());
        assert!(safe_join(Path::new("/tmp/dest"), "a\0b.eml").is_err());
    }

    #[test]
    fn an_ordinary_entry_path_joins_under_the_root() {
        let joined = safe_join(Path::new("/tmp/dest"), "cur/12.rmail:2,S").unwrap();
        assert_eq!(joined, Path::new("/tmp/dest/cur/12.rmail:2,S"));
    }
}
