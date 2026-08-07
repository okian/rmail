//! Tests for "open HTML in browser": the file must be private, the path must
//! never become shell syntax, and the document must not be able to phone
//! home.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use super::*;

/// A unique scratch directory. There is no `tempfile` dependency in this
/// workspace (see `rmail_core::storage::tests`' hand-rolled equivalent).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rmail-tui-html-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An opener that records rather than launching anything.
#[derive(Default)]
struct Recorder {
    opened: Mutex<Vec<PathBuf>>,
}

impl Opener for Recorder {
    fn open(&self, path: &Path) -> Result<()> {
        self.opened.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
}

#[test]
fn the_temp_file_is_not_readable_by_anyone_else() {
    // The default `File::create` mode is `0666 & !umask`, which with the
    // usual `022` umask is `0644` — every account on the machine could read
    // the user's mail.
    let scratch = Scratch::new("mode");
    let path = scratch.join("private.html");
    write_private(&path, b"<p>secret</p>").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode was {mode:o}, expected 600");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "<p>secret</p>");
}

#[test]
fn the_mode_is_set_at_creation_not_afterwards() {
    // A chmod after the fact would leave a window where the file exists, has
    // the mail in it, and is world-readable. Proving "never" from outside the
    // process is not possible; what is provable is that the very first
    // observation of the file — the failing `create_new` below, which can
    // only succeed if the file already exists — sees `0600`.
    let scratch = Scratch::new("create");
    let path = scratch.join("first.html");
    write_private(&path, b"x").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let second = write_private(&path, b"y");
    assert!(
        second.is_err(),
        "create_new must refuse an existing path rather than reuse or follow it"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "x",
        "the existing file was not overwritten"
    );
}

#[test]
fn write_private_refuses_to_follow_a_planted_symlink() {
    // The classic temp-file race: an attacker who can guess the path plants a
    // symlink to somewhere valuable and the write follows it. `create_new`
    // fails closed on any existing path, symlinks included.
    let scratch = Scratch::new("symlink");
    let victim = scratch.join("victim");
    std::fs::write(&victim, b"original").unwrap();
    let link = scratch.join("link.html");
    std::os::unix::fs::symlink(&victim, &link).unwrap();

    assert!(write_private(&link, b"attacker wrote this").is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
}

#[test]
fn opener_passes_the_path_as_one_argv_entry_never_through_a_shell() {
    // The invariant `rmail_core::hooks` documents at length, applied here: a
    // path that reaches a shell is a path whose metacharacters are syntax.
    // The stand-in "browser" records `$1` verbatim; if the path had been
    // interpolated into a command string, `$1` would be a fragment of it and
    // the `touch` would have run.
    let scratch = Scratch::new("argv");
    let recorded = scratch.join("argv.txt");
    let script = scratch.join("fake-open.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", recorded.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Every way a shell could be told to run something, in one filename:
    // statement separator, command substitution, backticks, a comment. A
    // filename cannot contain `/`, so the payloads are deliberately relative
    // — checked for below in both plausible working directories.
    let hostile = scratch.join("evil'; touch pwned; $(touch pwned) `touch pwned` #.html");
    std::fs::write(&hostile, b"<p>hi</p>").unwrap();

    CommandOpener::new(script.to_string_lossy().into_owned())
        .open(&hostile)
        .unwrap();
    // `open` returns as soon as the handler is *launched*, so wait for the
    // script to have written its argument before reading it.
    for _ in 0..500 {
        if recorded.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        std::fs::read_to_string(&recorded).unwrap(),
        hostile.to_string_lossy(),
        "the child received the path verbatim, as a single argument — a shell \
         would have word-split it at the `;` and eaten the substitutions"
    );
    for suspect in [scratch.join("pwned"), PathBuf::from("pwned")] {
        assert!(
            !suspect.exists(),
            "{} exists: the path's shell metacharacters executed",
            suspect.display()
        );
    }
}

#[test]
fn an_opener_that_cannot_be_spawned_is_reported_rather_than_ignored() {
    let opener = CommandOpener::new("rmail-no-such-program-exists");
    let error = opener
        .open(Path::new("/tmp/whatever.html"))
        .expect_err("spawning a missing program should fail");
    assert!(error.to_string().contains("rmail-no-such-program-exists"));
}

#[test]
fn the_opener_returns_without_waiting_for_the_handler_to_exit() {
    // `.status()` here would hold the calling blocking task open for as long
    // as the user's browser is open, and a runtime shutdown waits for its
    // blocking pool — so `mail tui` would refuse to quit until they closed
    // it. This asserts the launch returns promptly even though the child
    // sleeps far longer than the deadline.
    let scratch = Scratch::new("detach");
    let script = scratch.join("slow-open.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let started = std::time::Instant::now();
    CommandOpener::new(script.to_string_lossy().into_owned())
        .open(&scratch.join("anything.html"))
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "open() waited for the handler: {elapsed:?}"
    );
}

#[test]
fn sweep_removes_files_from_this_process_and_leaves_everyone_elses_alone() {
    let opener = Recorder::default();
    let mine = open_in_browser(555, "<p>private</p>", &opener).unwrap();

    // A file another `mail tui` would have written: same prefix, different
    // pid. Deleting it would pull an open message out from under that session.
    let theirs = std::env::temp_dir().join(format!("rmail-msg-{}-0-1.html", u32::MAX));
    std::fs::write(&theirs, b"someone else's mail").unwrap();

    assert!(mine.exists());
    sweep();

    assert!(
        !mine.exists(),
        "decoded mail was left in the temp directory"
    );
    assert!(theirs.exists(), "another session's file was deleted");
    let _ = std::fs::remove_file(&theirs);
}

#[test]
fn the_document_denies_every_fetch_including_tracking_pixels() {
    let wrapped = wrap("<img src=\"https://tracker.example/pixel.gif?id=42\">");

    let csp_at = wrapped
        .find("Content-Security-Policy")
        .expect("a CSP meta tag");
    let body_at = wrapped.find("<body>").expect("a body");
    assert!(
        csp_at < body_at,
        "the policy must be declared before anything that could fetch"
    );
    assert!(wrapped.contains("default-src 'none'"), "{wrapped}");
    assert!(
        wrapped.contains("img-src data:") && !wrapped.contains("img-src *"),
        "remote images — the read receipt of HTML mail — stay blocked: {wrapped}"
    );
    assert!(wrapped.contains("form-action 'none'"));
    assert!(
        wrapped.contains("pixel.gif"),
        "the message's own markup is still there, just declawed"
    );
}

#[test]
fn open_in_browser_writes_the_wrapped_document_privately_and_hands_it_over() {
    let opener = Recorder::default();
    let path = open_in_browser(1234, "<p>hello</p>", &opener).unwrap();

    let opened = opener.opened.lock().unwrap();
    assert_eq!(
        opened.as_slice(),
        std::slice::from_ref(&path),
        "the file written is the file opened"
    );

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("<p>hello</p>"));
    assert!(contents.contains("Content-Security-Policy"));
    assert!(
        path.to_string_lossy().contains("1234"),
        "the file names the message it came from: {}",
        path.display()
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn two_opens_of_the_same_message_do_not_collide() {
    let opener = Recorder::default();
    let first = open_in_browser(7, "<p>a</p>", &opener).unwrap();
    let second = open_in_browser(7, "<p>b</p>", &opener).unwrap();
    assert_ne!(
        first, second,
        "create_new would have failed on a reused path"
    );
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
}
