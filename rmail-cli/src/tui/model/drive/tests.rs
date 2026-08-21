//! Loop-level tests: a whole session driven end to end with a fake executor
//! and a fake painter, so the "never blocks" guarantee is asserted against
//! the real loop rather than argued from its shape.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Notify;

use super::*;
use crate::tui::model::{Effect, Folder, Key, MessageRow};

/// Records what it was asked to do and answers instantly.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<Cmd>>,
}

impl CmdExec for Recorder {
    fn exec(&self, cmd: Cmd, _out: UnboundedSender<Msg>) {
        self.seen.lock().unwrap().push(cmd);
    }
}

/// Holds every command open until `release` is signalled — a daemon that has
/// accepted the request and not answered yet.
struct Stalled {
    release: Arc<Notify>,
    started: Arc<AtomicUsize>,
}

impl CmdExec for Stalled {
    fn exec(&self, _cmd: Cmd, out: UnboundedSender<Msg>) {
        self.started.fetch_add(1, Ordering::SeqCst);
        let release = Arc::clone(&self.release);
        // Spawning is what an executor is *for*: `exec` returns immediately
        // and the answer arrives later, on the same channel as key presses.
        tokio::spawn(async move {
            release.notified().await;
            let _ = out.send(Msg::Done {
                label: "archived".to_owned(),
                result: Ok(Effect::Removed(10)),
            });
        });
    }
}

fn row(id: i64) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: None,
        flags: Vec::new(),
        has_attachments: false,
        has_note: false,
        to: None,
        tags: Vec::new(),
        ai: None,
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.folders = vec![
        Folder {
            id: 1,
            name: "INBOX".to_owned(),
            message_count: 3,
        },
        Folder {
            id: 2,
            name: "Archive".to_owned(),
            message_count: 0,
        },
    ];
    model.open_folder = Some(1);
    model.messages = vec![row(10), row(11), row(12)];
    model
}

#[tokio::test]
async fn the_first_frame_is_painted_before_any_message_is_processed() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder::default();

    let seen = Arc::clone(&frames);
    tx.send(Msg::Key(Key::Char('q'))).unwrap();
    let model = run_loop(Model::new(), &mut rx, &tx, &recorder, move |model| {
        seen.lock().unwrap().push(model.quit);
        Ok(())
    })
    .await
    .unwrap();

    assert!(model.quit);
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 2, "one frame before the key, one after it");
    assert!(
        !frames[0],
        "the first frame was painted before anything was handled"
    );
    assert!(frames[1], "the second reflects the quit key");
}

#[tokio::test]
async fn the_loop_stops_when_the_model_quits_even_with_messages_still_queued() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let recorder = Recorder::default();

    tx.send(Msg::Key(Key::Char('q'))).unwrap();
    // Queued behind the quit; the loop must not process it.
    tx.send(Msg::Key(Key::Char('j'))).unwrap();

    let mut model = loaded();
    model.message_idx = 0;
    let model = run_loop(model, &mut rx, &tx, &recorder, |_| Ok(()))
        .await
        .unwrap();

    assert!(model.quit);
    assert_eq!(model.message_idx, 0, "the queued j was never handled");
}

#[tokio::test]
async fn commands_returned_by_update_reach_the_executor() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let recorder = Recorder::default();

    tx.send(Msg::Key(Key::Char('a'))).unwrap();
    tx.send(Msg::Key(Key::Char('q'))).unwrap();
    run_loop(loaded(), &mut rx, &tx, &recorder, |_| Ok(()))
        .await
        .unwrap();

    let seen = recorder.seen.lock().unwrap();
    assert_eq!(
        *seen,
        vec![Cmd::Move {
            message_id: 10,
            dest_mailbox_id: 2,
            label: "archived".to_owned(),
        }]
    );
}

#[tokio::test]
async fn stays_responsive_while_a_request_is_outstanding() {
    // The load-bearing guarantee: prd.md requires the UI never block on sync
    // or AI. Here a mutation is accepted and deliberately never answered. If
    // the loop awaited it — anywhere — the key presses behind it would not be
    // handled and no frame would paint until it completed.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let started = Arc::new(AtomicUsize::new(0));
    let exec = Stalled {
        release: Arc::clone(&release),
        started: Arc::clone(&started),
    };

    // (message_idx, inflight, rows) at each paint.
    let frames = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&frames);

    let sender = tx.clone();
    let driver = tokio::spawn(async move {
        run_loop(loaded(), &mut rx, &tx, &exec, move |model| {
            seen.lock()
                .unwrap()
                .push((model.message_idx, model.inflight, model.messages.len()));
            Ok(())
        })
        .await
    });

    sender.send(Msg::Key(Key::Char('a'))).unwrap(); // archive: stalls
    sender.send(Msg::Key(Key::Char('j'))).unwrap();
    sender.send(Msg::Key(Key::Char('j'))).unwrap();

    // Wait for the UI to have moved twice *while the request is open*.
    let moved_while_busy = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let hit = frames
                .lock()
                .unwrap()
                .iter()
                .any(|(idx, inflight, _)| *idx == 2 && *inflight == 1);
            if hit {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        moved_while_busy.is_ok(),
        "the cursor never moved while a request was outstanding: {:?}",
        frames.lock().unwrap()
    );
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the RPC really did start"
    );

    // Now let the daemon answer; the result folds in exactly as it would have
    // had it been fast. `notify_one` rather than `notify_waiters` because it
    // stores a permit: the spawned task has been created but may not have
    // reached its `await` yet, and a lost wakeup would hang this test.
    release.notify_one();
    // Wait for the *effect*, not merely for `inflight` to read zero: it
    // already did on the very first frame, before the archive was even asked
    // for, so that condition would have been satisfied by the wrong frame and
    // the quit below would have raced the answer.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if frames
                .lock()
                .unwrap()
                .iter()
                .any(|(_, inflight, rows)| *inflight == 0 && *rows == 2)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the stalled result never arrived");

    sender.send(Msg::Key(Key::Char('q'))).unwrap();
    let model = tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("the loop never finished")
        .expect("the loop task panicked")
        .expect("the loop errored");

    assert!(model.quit);
    assert_eq!(
        model.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![11, 12],
        "the archived row was removed once the answer landed"
    );
}

#[tokio::test]
async fn a_paint_failure_ends_the_loop_instead_of_accepting_blind_keystrokes() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let recorder = Recorder::default();
    tx.send(Msg::Key(Key::Char('j'))).unwrap();

    let result = run_loop(loaded(), &mut rx, &tx, &recorder, |_| {
        Err(anyhow::anyhow!("the terminal went away"))
    })
    .await;

    let error = result.expect_err("a paint failure must not be swallowed");
    assert!(error.to_string().contains("terminal went away"));
}
