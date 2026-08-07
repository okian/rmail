//! The event loop that drives [`Model`] — still with no terminal in sight.
//!
//! [`run_loop`] owns the only place the three halves of the TUI meet: it
//! takes messages off one channel, folds each into the model with
//! [`update`], hands the resulting [`Cmd`]s to a [`CmdExec`], and asks its
//! caller to paint. It knows nothing about crossterm, ratatui or tonic — the
//! painter is a closure, the executor is a trait — which is what lets its
//! tests drive a whole session, including background work that completes out
//! of order, without a terminal or a daemon.
//!
//! # Why the loop cannot block on the network, structurally
//!
//! [`CmdExec::exec`] is **not** `async`. It takes a [`Cmd`] and a sender and
//! returns immediately; the only thing it can usefully do is spawn a task
//! that eventually sends [`Msg`]s back. So there is no way to express
//! "await this RPC before the next frame" from inside the loop: the loop's
//! single await point is `messages.recv()`, and key presses arrive on that
//! same channel as everything else. A slow `MailService.List`, a stalled
//! sync, an AI call that takes ten seconds — all of them are just messages
//! that have not arrived yet, and none of them can stop `j` from moving the
//! cursor in the meantime. `tests::stays_responsive_while_a_request_is_outstanding`
//! is the proof: it holds a mutation open indefinitely and asserts frames
//! keep painting with a moving cursor while `inflight` is non-zero.
//!
//! # Painting
//!
//! The first frame is painted *before* the first `recv`, so startup shows the
//! chrome immediately rather than after the account/folder/message round
//! trips (prd.md budgets 200 ms for TUI startup; the round trips are not on
//! that path). After that, one paint per message. A message that changes
//! nothing still repaints, which is cheap — ratatui diffs against the
//! previous buffer and writes only what changed.

#[cfg(test)]
mod tests;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::{update, Cmd, Model, Msg};

/// Performs a [`Cmd`] somewhere other than the event loop.
///
/// Implementations must return immediately: spawn the work, send [`Msg`]s to
/// `out` when it finishes. An implementation that blocks in `exec` puts the
/// UI back on the network's critical path, which is the one thing this
/// design exists to prevent.
pub trait CmdExec: Send + Sync {
    /// Start `cmd`, reporting its outcome to `out` later.
    fn exec(&self, cmd: Cmd, out: UnboundedSender<Msg>);
}

/// Run the TUI until the model asks to quit or every sender is dropped.
///
/// `paint` is called once before the first message and once per message after
/// it. A paint failure ends the loop with that error — a TUI that cannot draw
/// has nothing useful left to do, and continuing would silently accept
/// keystrokes against a frozen screen.
///
/// # Errors
///
/// Whatever `paint` returns.
pub async fn run_loop<P>(
    mut model: Model,
    messages: &mut UnboundedReceiver<Msg>,
    out: &UnboundedSender<Msg>,
    exec: &dyn CmdExec,
    mut paint: P,
) -> anyhow::Result<Model>
where
    P: FnMut(&Model) -> anyhow::Result<()>,
{
    paint(&model)?;

    while let Some(msg) = messages.recv().await {
        for cmd in update(&mut model, msg) {
            exec.exec(cmd, out.clone());
        }
        paint(&model)?;
        if model.quit {
            break;
        }
    }

    Ok(model)
}
