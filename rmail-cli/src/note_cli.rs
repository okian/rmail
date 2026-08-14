//! `mail note`/`mail notes` — thin gRPC-client verbs over `NoteService`
//! (task 56).
//!
//! # The `$EDITOR` flow
//!
//! `mail note add <id>` with no `-m` writes the note's starting text (empty,
//! for a new note) to a private temp file, launches `$EDITOR` on it
//! (falling back to `vi` when unset — the same default `git commit` uses),
//! waits for it to exit, and reads the result back. An empty buffer (after
//! trimming) aborts without calling `AddNote` at all — the same "nothing
//! typed, nothing happened" contract `git commit` gives an empty commit
//! message, and cheaper than a round trip to the daemon just to have
//! [`rmail_core::notes::NoteStore::add`] reject an empty body.
//!
//! `mail note edit <note_id>` intentionally does **not** offer the same
//! interactive flow: `EditNoteRequest` replaces a note's body wholesale, and
//! `NoteService` has no `GetNote` RPC to pre-fill an editor buffer with the
//! note's current text (only `ListNotes`, scoped by target, not by note id).
//! Opening `$EDITOR` on an empty buffer for an *edit* would silently invite
//! "the new text IS the whole note" to be misread as "type what you want to
//! add" and lose the rest — `edit` requires `-m` outright instead of
//! guessing.
//!
//! # Target parsing
//!
//! Every subcommand takes a bare id plus a `--thread` flag rather than a
//! combined `message:<id>`/`thread:<id>` syntax — matching how every other
//! id-taking verb in this CLI (`mail ai process <message_id>`, `mail token
//! revoke <id>`) already takes a bare integer, and avoiding a second small
//! parser for what `--thread` already says unambiguously.

use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use rmail_proto::v1::note_service_client::NoteServiceClient;
use rmail_proto::v1::note_target::Of;
use rmail_proto::v1::{
    AddNoteRequest, DeleteNoteRequest, EditNoteRequest, ListNotesRequest, Note, NoteAuthor,
    NoteTarget,
};

/// `mail note <action>`.
#[derive(Debug, Subcommand)]
pub enum NoteAction {
    /// Attach a new note to a message (or, with `--thread`, a thread).
    /// Opens `$EDITOR` unless `-m`/`--message` is given.
    Add {
        /// Message id, or (with `--thread`) a thread id.
        id: i64,
        /// Attach to the thread `id` names, instead of the message it names.
        #[arg(long)]
        thread: bool,
        /// Note text, given inline instead of via `$EDITOR`.
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
    },
    /// Replace an existing note's body (last-write-wins — see
    /// `NoteService.EditNote`'s own doc comment).
    Edit {
        /// Note id (as printed by `mail note add`/`mail notes`).
        note_id: i64,
        /// The note's new text.
        #[arg(short = 'm', long = "message", required = true)]
        message: String,
    },
    /// Delete a note by id.
    Rm {
        /// Note id.
        note_id: i64,
    },
}

/// `mail notes <id> [--thread]` — list a target's notes, newest first.
#[derive(Debug, clap::Args)]
pub struct NotesArgs {
    /// Message id, or (with `--thread`) a thread id.
    id: i64,
    /// List the thread `id` names, instead of the message it names.
    #[arg(long)]
    thread: bool,
}

/// Run a `mail note <action>` subcommand.
pub async fn dispatch(socket: &Path, action: NoteAction) -> Result<()> {
    match action {
        NoteAction::Add {
            id,
            thread,
            message,
        } => add(socket, id, thread, message).await,
        NoteAction::Edit { note_id, message } => edit(socket, note_id, message).await,
        NoteAction::Rm { note_id } => rm(socket, note_id).await,
    }
}

/// Run `mail notes <id> [--thread]`.
pub async fn list(socket: &Path, args: NotesArgs) -> Result<()> {
    let mut client = client(socket).await?;
    let target = to_target(args.id, args.thread);
    let response = client
        .list_notes(ListNotesRequest {
            target: Some(target),
        })
        .await
        .context("ListNotes RPC failed")?
        .into_inner();

    if response.notes.is_empty() {
        println!("no notes");
        return Ok(());
    }
    for note in &response.notes {
        print_note(note);
        println!();
    }
    Ok(())
}

async fn add(socket: &Path, id: i64, thread: bool, message: Option<String>) -> Result<()> {
    let body_md = match message {
        Some(text) => text,
        None => match edit_in_editor("").await? {
            Some(text) => text,
            None => {
                println!("empty note, aborting");
                return Ok(());
            }
        },
    };

    let mut client = client(socket).await?;
    let target = to_target(id, thread);
    let note = client
        .add_note(AddNoteRequest {
            idempotency_key: String::new(),
            target: Some(target),
            body_md,
            author: NoteAuthor::User as i32,
        })
        .await
        .context("AddNote RPC failed")?
        .into_inner();
    println!("note {} added", note.id);
    Ok(())
}

async fn edit(socket: &Path, note_id: i64, body_md: String) -> Result<()> {
    let mut client = client(socket).await?;
    let note = client
        .edit_note(EditNoteRequest { note_id, body_md })
        .await
        .context("EditNote RPC failed")?
        .into_inner();
    println!("note {} updated", note.id);
    Ok(())
}

async fn rm(socket: &Path, note_id: i64) -> Result<()> {
    let mut client = client(socket).await?;
    client
        .delete_note(DeleteNoteRequest { note_id })
        .await
        .context("DeleteNote RPC failed")?;
    println!("note {note_id} deleted");
    Ok(())
}

async fn client(socket: &Path) -> Result<NoteServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(NoteServiceClient::new(channel))
}

fn to_target(id: i64, thread: bool) -> NoteTarget {
    NoteTarget {
        of: Some(if thread {
            Of::ThreadId(id)
        } else {
            Of::MessageId(id)
        }),
    }
}

fn print_note(note: &Note) {
    let target = match note.target.as_ref().and_then(|t| t.of.as_ref()) {
        Some(Of::MessageId(id)) => format!("message {id}"),
        Some(Of::ThreadId(id)) => format!("thread {id}"),
        None => "(no target)".to_owned(),
    };
    let author = match note.author() {
        NoteAuthor::Ai => "ai",
        _ => "user",
    };
    println!("note {}  [{target}, {author}]", note.id);
    println!("{}", note.body_md);
}

// ---------------------------------------------------------------------------
// The `$EDITOR` flow
// ---------------------------------------------------------------------------

/// Write `initial` to a private temp file, run `$EDITOR` (or `vi`) on it,
/// and return the trimmed result — `None` if the buffer is empty (or
/// whitespace-only) on exit, the "aborted" case per this module's own docs.
///
/// # Errors
///
/// If the temp file cannot be written/read back, or the editor cannot be
/// launched or exits non-zero (a real editor failure, not "the user closed
/// without saving" — most editors exit 0 either way).
async fn edit_in_editor(initial: &str) -> Result<Option<String>> {
    let path = temp_note_path();
    tokio::fs::write(&path, initial)
        .await
        .with_context(|| format!("writing temp note file {}", path.display()))?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
    let status = tokio::process::Command::new(&editor)
        .arg(&path)
        .status()
        .await
        .with_context(|| format!("launching editor `{editor}` (set $EDITOR to override)"))?;

    if !status.success() {
        let _ = tokio::fs::remove_file(&path).await;
        bail!("editor `{editor}` exited with {status}");
    }

    let contents = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading back {}", path.display()))?;
    let _ = tokio::fs::remove_file(&path).await;

    let trimmed = contents.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

/// A unique temp file path for one `$EDITOR` session. No `tempfile`
/// dependency in this workspace (see `rmail-core::storage::tests`'s own
/// hand-rolled equivalent) -- pid plus a monotonic counter is enough to
/// avoid colliding with a concurrent `mail note add` in another terminal.
fn temp_note_path() -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-note-{pid}-{n}.md"))
}
