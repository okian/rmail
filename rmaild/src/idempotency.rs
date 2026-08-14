//! The gRPC-boundary half of the mutating-RPC replay fence.
//!
//! [`rmail_core::idempotency`] owns the storage rule (claim before the act,
//! record after, release on failure, refuse an unfinished claim) and the
//! reasoning behind it. This module is the thin adapter that turns it into
//! something a tonic handler can wrap one line of work in:
//!
//! ```ignore
//! idempotency::guard(&self.idempotency, METHOD, &req.idempotency_key, &req, async {
//!     self.store.move_message(id, dest).await?;
//!     Ok(())
//! })
//! .await
//! ```
//!
//! # Why the *encoded request* is what gets hashed
//!
//! prd.md's rule is "same key+hash replays the cached response", and the hash
//! has to cover the whole call or the fence is worse than none: a key reused
//! with `dest_mailbox_id` changed would otherwise replay a success for a move
//! that never happened. Hashing the encoded protobuf covers every field
//! including ones added later, which a hand-written field list would silently
//! stop covering the day someone extends the message.
//!
//! The `idempotency_key` field is inside the hashed bytes. That is harmless —
//! a retry carries the same key by definition — and it is one fewer piece of
//! per-RPC bookkeeping to get wrong.
//!
//! Protobuf encoding is not canonical in general, but it is deterministic for
//! a given message value produced by a given prost build, and both sides of a
//! comparison here are encoded by the same daemon process. A daemon upgrade
//! that changed the encoding would at worst turn a replay into an
//! `ALREADY_EXISTS`, which fails closed.
//!
//! # Opt-in
//!
//! An empty key means "no fence": the handler runs exactly as it did before.
//! Existing clients that do not know about the field are unaffected, which is
//! what makes adding the field additive rather than a behaviour change.
//
// `tonic::Status` is intentionally the error type across a gRPC boundary; its
// size makes `result_large_err` fire on every `Result<_, Status>` here, so the
// lint is allowed for this module exactly as it is for the service modules.
#![allow(clippy::result_large_err)]

use std::future::Future;

use prost::Message as _;
use rmail_core::idempotency::{Claim, IdempotencyStore};
use rmail_core::Error;
use tonic::Status;

/// A response that can be cached and replayed byte-for-byte.
///
/// A narrow trait rather than a blanket `prost::Message` bound because the
/// most important RPCs on this path return `google.protobuf.Empty`, which
/// tonic models as `()` — not a `prost::Message` at all.
pub(crate) trait Replay: Sized {
    /// The bytes to store.
    fn encode_replay(&self) -> Vec<u8>;
    /// Rebuild from stored bytes.
    ///
    /// # Errors
    /// A [`Status`] if the stored bytes do not decode — a corrupted or
    /// migrated cache entry, which is a server-side problem and never the
    /// caller's.
    fn decode_replay(bytes: &[u8]) -> Result<Self, Status>;
}

impl Replay for () {
    fn encode_replay(&self) -> Vec<u8> {
        Vec::new()
    }
    fn decode_replay(_bytes: &[u8]) -> Result<Self, Status> {
        Ok(())
    }
}

impl Replay for rmail_proto::v1::OutboxEntry {
    fn encode_replay(&self) -> Vec<u8> {
        self.encode_to_vec()
    }
    fn decode_replay(bytes: &[u8]) -> Result<Self, Status> {
        Self::decode(bytes).map_err(|error| {
            tracing::error!(%error, "a cached idempotent response did not decode");
            Status::from(Error::internal("cached response could not be decoded"))
        })
    }
}

/// Run `work` under the replay fence named by `key`, or replay an identical
/// earlier call.
///
/// `method` is the fully-qualified gRPC method path; it is folded into the
/// request hash so one key cannot be reused across two RPCs.
///
/// # Errors
///
/// Whatever `work` returns, plus the fence's own refusals: `ALREADY_EXISTS`
/// for a key reused with a different payload, `ABORTED` for a retry against an
/// unfinished attempt, `INVALID_ARGUMENT` for a malformed key.
#[tracing::instrument(
    skip(store, key, request, work),
    fields(method = method, idempotency = tracing::field::Empty)
)]
pub(crate) async fn guard<Req, Resp, Fut>(
    store: &IdempotencyStore,
    method: &str,
    key: &str,
    request: &Req,
    work: Fut,
) -> Result<Resp, Status>
where
    Req: prost::Message,
    Resp: Replay,
    Fut: Future<Output = Result<Resp, Status>>,
{
    // The one field an operator wants when a client reports that its retry
    // "did nothing" (or, worse, happened twice): which of the three decisions
    // this call took. Recorded on the span rather than logged, so it is
    // attached to the request trace and costs nothing when spans are off.
    let span = tracing::Span::current();
    if key.is_empty() {
        span.record("idempotency", "unfenced");
        return work.await;
    }

    let claim = store
        .claim(key, method, &request.encode_to_vec())
        .await
        .inspect_err(|error| {
            span.record("idempotency", error.reason().as_str());
        })
        .map_err(Status::from)?;
    if let Claim::Replay(bytes) = claim {
        span.record("idempotency", "replayed");
        return Resp::decode_replay(&bytes);
    }
    span.record("idempotency", "fresh");

    match work.await {
        Ok(response) => {
            // A failure to record leaves the claim unfinished, so the *next*
            // retry is refused rather than re-applied. That is the safe
            // direction, and it must not fail the call that already succeeded.
            if let Err(error) = store.record(key, response.encode_replay()).await {
                tracing::error!(%error, method, "could not record an idempotent response");
            }
            Ok(response)
        }
        Err(status) => {
            // See `rmail_core::idempotency`'s module docs: a mutation that
            // returned an error did not apply, so the key must become
            // retryable again. Failing to release only makes the next retry
            // ABORTED, which is again the safe direction.
            if let Err(error) = store.release(key).await {
                tracing::error!(%error, method, "could not release an idempotency claim");
            }
            Err(status)
        }
    }
}
