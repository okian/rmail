//! A `tower` layer that wraps every gRPC request in a tracing span.
//!
//! This is the cross-cutting request-context concern promised by the telemetry
//! baseline (task 4): each RPC opens a span carrying a per-process `request_id`
//! and the RPC path, with `account`/`mailbox` left empty for handlers to
//! [`tracing::Span::record`] once known. The span's field names mirror
//! [`rmail_core::telemetry`]'s `FIELD_*` constants.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tracing::instrument::Instrumented;
use tracing::Instrument;

/// Layer that installs [`RequestTrace`] around a service.
#[derive(Clone, Default)]
pub struct RequestTraceLayer {
    counter: Arc<AtomicU64>,
}

impl RequestTraceLayer {
    /// Create a new layer with a fresh request counter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> Layer<S> for RequestTraceLayer {
    type Service = RequestTrace<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTrace {
            inner,
            counter: Arc::clone(&self.counter),
        }
    }
}

/// Service wrapper that opens a request span per call.
#[derive(Clone)]
pub struct RequestTrace<S> {
    inner: S,
    counter: Arc<AtomicU64>,
}

impl<S, B> Service<http::Request<B>> for RequestTrace<S>
where
    S: Service<http::Request<B>>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        // Per-process monotonic id — unique within a run, but resets on restart.
        // Sufficient for local correlation; a UUID/startup salt would make it
        // unique across restarts if logs are ever aggregated centrally.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        // The gRPC method is the request path, e.g. /rmail.v1.AccountService/Get.
        let span = tracing::info_span!(
            "rmail.rpc",
            request_id = %format_args!("req-{n}"),
            rpc = %req.uri().path(),
            account = tracing::field::Empty,
            mailbox = tracing::field::Empty,
        );
        self.inner.call(req).instrument(span)
    }
}
