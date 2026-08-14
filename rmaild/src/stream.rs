//! How a server-streamed RPC ends when the daemon cancels it.
//!
//! # The bug this exists to close
//!
//! A tonic server-stream that stops yielding items terminates the call with
//! `OK`. That is right when the stream ran out of things to say, and wrong
//! every other time — and the two are indistinguishable on the wire. The
//! symptom that made it concrete: `AiService.AskMailbox` cancelled mid-answer
//! ended `OK` with no `AskDone` frame, so `mail ask` printed half a sentence
//! and exited 0. The client had no way to know it was reading a truncated
//! answer, because "the model finished" and "the daemon stopped" looked the
//! same.
//!
//! Every streaming RPC here has the same shape — a producer task feeding a
//! bounded channel, giving up when its cancellation token fires — so every one
//! of them had the same hole.
//!
//! # The rule
//!
//! **A stream the daemon cancels ends with an error frame, never silently.**
//! The frame carries [`rmail_core::ErrorReason::Cancelled`], which is what a
//! client branches on to tell "incomplete" from "complete".
//!
//! It is best-effort by necessity: the token is cancelled by daemon shutdown
//! *and* (for the AI streams) by the client dropping the response, and in the
//! second case there is nobody left to tell — a closed channel simply drops
//! the frame.
//!
//! But "best-effort" must not mean "give up immediately". The cancel branch of
//! a `send` helper wins the `select!` mostly when the producer is *parked on a
//! full channel*, which is exactly when a bare `try_send` fails — so the frame
//! would be dropped in the one case it was written for. A consumer that is
//! momentarily behind is not a consumer that has gone away; that case is
//! already covered by the channel being closed. [`terminate_cancelled`]
//! therefore waits, but only up to [`TERMINATE_GRACE`], which is far inside
//! the window `serve_uds` leaves between cancelling `stopping` and dropping
//! the transport — so a stuck stream still cannot hold shutdown open.
//!
//! # Why `CANCELLED` and not `UNAVAILABLE`
//!
//! `UNAVAILABLE` is gRPC's "try again", and generic retry middleware treats it
//! that way. For a half-streamed AI answer that is an instruction to spend
//! money re-running a call that may have almost finished. `CANCELLED` says the
//! true thing — this call stopped early — and leaves whether to retry to a
//! client that knows what the call cost.

use std::time::Duration;

use rmail_core::Error;
use tokio::sync::mpsc::Sender;
use tonic::Status;

/// How long the terminal frame waits for room in a full channel.
///
/// Long enough for a client that is merely a little behind to take it, short
/// enough that a client that has stopped reading entirely cannot measurably
/// delay shutdown.
const TERMINATE_GRACE: Duration = Duration::from_millis(250);

/// The terminal status a cancelled stream ends with.
pub(crate) fn cancelled_status() -> Status {
    Status::from(Error::cancelled(
        "the daemon ended this stream before it completed; the result is partial",
    ))
}

/// Terminal frame for a stream the daemon cancelled, waiting at most
/// [`TERMINATE_GRACE`] for room — see the module docs on why it waits at all.
pub(crate) async fn terminate_cancelled<T>(tx: &Sender<Result<T, Status>>) {
    if tokio::time::timeout(TERMINATE_GRACE, tx.send(Err(cancelled_status())))
        .await
        .is_err()
    {
        tracing::debug!("cancelled stream could not deliver its terminal frame in time");
    }
}
