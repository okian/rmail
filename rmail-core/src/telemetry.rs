//! Tracing/observability baseline.
//!
//! Everything logs through `tracing`, never by writing to stdout/stderr
//! directly. [`init`] installs the process-wide subscriber (env-filtered
//! levels, text or JSON), and the span helpers ([`request_span`],
//! [`request_span_with`]) stamp the structured fields the rest of the service
//! correlates on: request id, account, and mailbox.

use std::str::FromStr;

use tracing::field::Empty;
use tracing::Span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Environment variable selecting the log output format (`text` or `json`).
pub const LOG_FORMAT_ENV: &str = "RMAIL_LOG_FORMAT";

/// Structured-field name for the correlation/request id.
pub const FIELD_REQUEST_ID: &str = "request_id";
/// Structured-field name for the account.
pub const FIELD_ACCOUNT: &str = "account";
/// Structured-field name for the mailbox.
pub const FIELD_MAILBOX: &str = "mailbox";

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable text (default).
    #[default]
    Text,
    /// Structured single-line JSON (for log shippers).
    Json,
}

impl LogFormat {
    /// Resolve the format from [`LOG_FORMAT_ENV`]; unset or unparsable → text.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var(LOG_FORMAT_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    }
}

impl FromStr for LogFormat {
    type Err = TelemetryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "text" | "plain" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            other => Err(TelemetryError::UnknownFormat(other.to_owned())),
        }
    }
}

/// Errors from telemetry setup.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The configured log format string was not recognized.
    #[error("unknown log format {0:?} (use \"text\" or \"json\")")]
    UnknownFormat(String),

    /// A global subscriber was already installed for this process.
    #[error("a tracing subscriber is already initialized: {0}")]
    AlreadyInitialized(#[from] tracing_subscriber::util::TryInitError),
}

/// Install the process-wide `tracing` subscriber.
///
/// Levels are controlled by the `RUST_LOG` env filter; an unset or unparsable
/// filter falls back to `info`. `format` selects human-readable text or
/// structured JSON output. Call once, early, at binary startup.
///
/// # Errors
///
/// Returns [`TelemetryError::AlreadyInitialized`] if a global subscriber has
/// already been installed.
pub fn init(format: LogFormat) -> Result<(), TelemetryError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let result = match format {
        LogFormat::Text => tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer())
            .try_init(),
        LogFormat::Json => tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().flatten_event(true))
            .try_init(),
    };

    result.map_err(TelemetryError::from)
}

/// A request-scoped span carrying `request_id`, with `account` and `mailbox`
/// left empty for the caller to [`Span::record`] as they become known.
///
/// The literal field names below are the public contract published as
/// [`FIELD_REQUEST_ID`], [`FIELD_ACCOUNT`], and [`FIELD_MAILBOX`]; keep them in
/// sync (`info_span!` requires literal identifiers, so they can't reference the
/// constants directly).
#[must_use]
pub fn request_span(request_id: &str) -> Span {
    tracing::info_span!(
        "rmail.request",
        request_id = %request_id,
        account = Empty,
        mailbox = Empty,
    )
}

/// A request-scoped span with `request_id` plus any already-known `account`
/// and `mailbox` fields recorded.
#[must_use]
pub fn request_span_with(request_id: &str, account: Option<&str>, mailbox: Option<&str>) -> Span {
    let span = request_span(request_id);
    if let Some(account) = account {
        span.record(FIELD_ACCOUNT, account);
    }
    if let Some(mailbox) = mailbox {
        span.record(FIELD_MAILBOX, mailbox);
    }
    span
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// A `MakeWriter` that appends everything into a shared buffer.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| io::Error::other("log buffer poisoned"))?;
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn log_format_parses_and_rejects_unknown() {
        assert_eq!(LogFormat::from_str("json").unwrap(), LogFormat::Json);
        assert_eq!(LogFormat::from_str("TEXT").unwrap(), LogFormat::Text);
        assert_eq!(LogFormat::from_str("").unwrap(), LogFormat::Text);
        assert!(matches!(
            LogFormat::from_str("yaml"),
            Err(TelemetryError::UnknownFormat(_))
        ));
    }

    #[test]
    fn events_flow_through_subscriber_with_structured_fields() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = BufWriter(buf.clone());

        // A scoped (thread-local) JSON subscriber — no global state, so this is
        // hermetic and does not touch stdout.
        let subscriber = tracing_subscriber::registry()
            .with(fmt::layer().json().flatten_event(true).with_writer(writer));

        tracing::subscriber::with_default(subscriber, || {
            let span = request_span_with("req-123", Some("Personal"), Some("INBOX"));
            let _entered = span.enter();
            tracing::info!(target: "rmail_core::telemetry", event_kind = "unit_test", "hello");
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!captured.is_empty(), "nothing was captured");
        // The event and every structured field must have flowed through the
        // subscriber (not stdout).
        assert!(captured.contains("hello"), "message missing: {captured}");
        assert!(
            captured.contains("req-123"),
            "request_id missing: {captured}"
        );
        assert!(captured.contains("Personal"), "account missing: {captured}");
        assert!(captured.contains("INBOX"), "mailbox missing: {captured}");
        assert!(
            captured.contains("unit_test"),
            "event field missing: {captured}"
        );
    }

    #[test]
    fn request_span_records_optional_fields() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = BufWriter(buf.clone());
        let subscriber = tracing_subscriber::registry()
            .with(fmt::layer().json().flatten_event(true).with_writer(writer));

        tracing::subscriber::with_default(subscriber, || {
            // No account/mailbox known yet at span creation.
            let span = request_span("req-abc");
            span.record(FIELD_ACCOUNT, "Work");
            let _entered = span.enter();
            tracing::warn!("late field recorded");
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("req-abc"));
        assert!(captured.contains("Work"));
    }
}
