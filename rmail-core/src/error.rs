//! The core domain error model and its mapping to `tonic::Status`.
//!
//! Library and domain code returns [`Error`] (a `thiserror` enum) — never
//! `anyhow`, which is reserved for binary top levels. At the gRPC boundary an
//! [`Error`] converts to a [`tonic::Status`] carrying a `google.rpc.ErrorInfo`
//! with a **stable** [`ErrorReason`]. Clients branch on `reason` (and `domain`),
//! never on the human-readable message.

use std::collections::HashMap;

use tonic::{Code, Status};
use tonic_types::{ErrorDetails, StatusExt};

use crate::config::ConfigError;

/// The `ErrorInfo.domain` for every rmail error. Stable across a major version.
pub const ERROR_DOMAIN: &str = "rmail.v1";

/// A stable, machine-branchable error reason attached to every [`Status`] via
/// `google.rpc.ErrorInfo`.
///
/// The string form (see [`ErrorReason::as_str`]) is the contract clients depend
/// on; it never changes for a given semantic within a major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorReason {
    /// Missing or invalid credentials / auth token.
    Unauthenticated,
    /// Caller lacks the required capability scope.
    PermissionDenied,
    /// A requested resource does not exist.
    NotFound,
    /// System is not in the required state (daemon offline, not synced, no AI key).
    FailedPrecondition,
    /// An upstream dependency (IMAP/SMTP/provider) is unreachable; retryable.
    Unavailable,
    /// A rate, size, or budget limit was exceeded.
    ResourceExhausted,
    /// A deadline elapsed before the operation completed.
    DeadlineExceeded,
    /// The resource already exists / idempotency replay with a differing payload.
    AlreadyExists,
    /// A cursor is past retention (e.g. `WatchEvents` resume gap); carries the
    /// oldest still-available position so the client can resync.
    OutOfRange,
    /// Client input was malformed.
    InvalidArgument,
    /// An unexpected internal error (the detail stays server-side; the boundary
    /// returns a generic message).
    Internal,
}

impl ErrorReason {
    /// Every reason, for exhaustive iteration in tests and tooling.
    pub const ALL: [ErrorReason; 11] = [
        ErrorReason::Unauthenticated,
        ErrorReason::PermissionDenied,
        ErrorReason::NotFound,
        ErrorReason::FailedPrecondition,
        ErrorReason::Unavailable,
        ErrorReason::ResourceExhausted,
        ErrorReason::DeadlineExceeded,
        ErrorReason::AlreadyExists,
        ErrorReason::OutOfRange,
        ErrorReason::InvalidArgument,
        ErrorReason::Internal,
    ];

    /// The stable wire string placed in `ErrorInfo.reason`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorReason::Unauthenticated => "UNAUTHENTICATED",
            ErrorReason::PermissionDenied => "PERMISSION_DENIED",
            ErrorReason::NotFound => "NOT_FOUND",
            ErrorReason::FailedPrecondition => "FAILED_PRECONDITION",
            ErrorReason::Unavailable => "UNAVAILABLE",
            ErrorReason::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorReason::DeadlineExceeded => "DEADLINE_EXCEEDED",
            ErrorReason::AlreadyExists => "ALREADY_EXISTS",
            ErrorReason::OutOfRange => "OUT_OF_RANGE",
            ErrorReason::InvalidArgument => "INVALID_ARGUMENT",
            ErrorReason::Internal => "INTERNAL",
        }
    }

    /// The gRPC status code this reason maps to.
    #[must_use]
    pub const fn code(self) -> Code {
        match self {
            ErrorReason::Unauthenticated => Code::Unauthenticated,
            ErrorReason::PermissionDenied => Code::PermissionDenied,
            ErrorReason::NotFound => Code::NotFound,
            ErrorReason::FailedPrecondition => Code::FailedPrecondition,
            ErrorReason::Unavailable => Code::Unavailable,
            ErrorReason::ResourceExhausted => Code::ResourceExhausted,
            ErrorReason::DeadlineExceeded => Code::DeadlineExceeded,
            ErrorReason::AlreadyExists => Code::AlreadyExists,
            ErrorReason::OutOfRange => Code::OutOfRange,
            ErrorReason::InvalidArgument => Code::InvalidArgument,
            ErrorReason::Internal => Code::Internal,
        }
    }
}

/// A convenient `Result` alias for domain code.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The core domain error. Every variant carries a human-readable message and
/// maps to a single [`ErrorReason`] (and thus one gRPC [`Code`]).
///
/// # Message contract
///
/// Except for [`Error::Internal`], a variant's `String` is emitted verbatim as
/// the `tonic::Status` message, so it **must be safe to expose to clients** — no
/// secrets, credentials, or raw upstream-error detail. When later tasks add
/// variants that wrap an upstream cause (IMAP/SMTP/SQLite/provider), keep a safe
/// summary in the message and carry the cause via a `#[source]` field for
/// logging only — never `to_string()` the source into the client message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Missing or invalid credentials / auth token.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    /// Caller lacks the required capability scope.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// A requested resource does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// System not in the required state (daemon offline, not synced, no AI key).
    #[error("failed precondition: {0}")]
    FailedPrecondition(String),

    /// An upstream dependency (IMAP/SMTP/provider) is unreachable; retryable.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// A rate, size, or budget limit was exceeded.
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

    /// A deadline elapsed before completion.
    #[error("deadline exceeded: {0}")]
    DeadlineExceeded(String),

    /// The resource already exists / idempotency replay with a differing payload.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// A cursor is past retention (e.g. a `WatchEvents` resume gap).
    #[error("out of range: {0}")]
    OutOfRange(String),

    /// Client input was malformed.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// An unexpected internal error.
    ///
    /// The detail is preserved on this value (via [`Display`](std::fmt::Display))
    /// for the caller to log, but the gRPC boundary substitutes a generic
    /// `"internal error"` message so implementation detail never reaches clients.
    /// Callers converting an `Internal` to a [`Status`] should log it first.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Construct an [`Error::Unauthenticated`].
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::Unauthenticated(message.into())
    }
    /// Construct an [`Error::PermissionDenied`].
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::PermissionDenied(message.into())
    }
    /// Construct an [`Error::NotFound`].
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }
    /// Construct an [`Error::FailedPrecondition`].
    pub fn failed_precondition(message: impl Into<String>) -> Self {
        Self::FailedPrecondition(message.into())
    }
    /// Construct an [`Error::Unavailable`].
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
    /// Construct an [`Error::ResourceExhausted`].
    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::ResourceExhausted(message.into())
    }
    /// Construct an [`Error::DeadlineExceeded`].
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::DeadlineExceeded(message.into())
    }
    /// Construct an [`Error::AlreadyExists`].
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::AlreadyExists(message.into())
    }
    /// Construct an [`Error::OutOfRange`].
    pub fn out_of_range(message: impl Into<String>) -> Self {
        Self::OutOfRange(message.into())
    }
    /// Construct an [`Error::InvalidArgument`].
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }
    /// Construct an [`Error::Internal`].
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// The stable [`ErrorReason`] for this error.
    #[must_use]
    pub const fn reason(&self) -> ErrorReason {
        match self {
            Error::Unauthenticated(_) => ErrorReason::Unauthenticated,
            Error::PermissionDenied(_) => ErrorReason::PermissionDenied,
            Error::NotFound(_) => ErrorReason::NotFound,
            Error::FailedPrecondition(_) => ErrorReason::FailedPrecondition,
            Error::Unavailable(_) => ErrorReason::Unavailable,
            Error::ResourceExhausted(_) => ErrorReason::ResourceExhausted,
            Error::DeadlineExceeded(_) => ErrorReason::DeadlineExceeded,
            Error::AlreadyExists(_) => ErrorReason::AlreadyExists,
            Error::OutOfRange(_) => ErrorReason::OutOfRange,
            Error::InvalidArgument(_) => ErrorReason::InvalidArgument,
            Error::Internal(_) => ErrorReason::Internal,
        }
    }

    /// The gRPC status code for this error.
    #[must_use]
    pub const fn code(&self) -> Code {
        self.reason().code()
    }

    /// Structured key/value context attached to the `Status` `ErrorInfo`.
    ///
    /// Empty today. Specific variants populate this as the contract grows — e.g.
    /// `oldest_seq` for [`ErrorReason::OutOfRange`] resume gaps, or retry/budget
    /// hints for [`ErrorReason::ResourceExhausted`] — so clients get structured
    /// context, not parsed message text.
    #[must_use]
    fn metadata(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Convert into a [`tonic::Status`] carrying `google.rpc.ErrorInfo`.
    #[must_use]
    pub fn into_status(self) -> Status {
        Status::from(self)
    }
}

impl From<ConfigError> for Error {
    fn from(err: ConfigError) -> Self {
        match err {
            // A missing config file means the system isn't ready to serve.
            ConfigError::NotFound(_) => Self::FailedPrecondition(err.to_string()),
            // A malformed value is a bad-input problem (e.g. a `SetConfig` RPC).
            ConfigError::Invalid(_) => Self::InvalidArgument(err.to_string()),
        }
    }
}

impl From<Error> for Status {
    fn from(err: Error) -> Self {
        let reason = err.reason();
        let metadata = err.metadata();

        // Internal error details are for logs, not clients — return a generic
        // message so implementation detail never crosses the boundary. Every
        // other variant's message is a client-safe string by contract.
        let message = match reason {
            ErrorReason::Internal => "internal error".to_owned(),
            _ => err.to_string(),
        };

        let mut details = ErrorDetails::new();
        details.set_error_info(reason.as_str(), ERROR_DOMAIN, metadata);
        Status::with_error_details(reason.code(), message, details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A representative error for each reason, in `ALL` order.
    fn sample(reason: ErrorReason) -> Error {
        match reason {
            ErrorReason::Unauthenticated => Error::unauthenticated("bad token"),
            ErrorReason::PermissionDenied => Error::permission_denied("scope mail.write required"),
            ErrorReason::NotFound => Error::not_found("message 42"),
            ErrorReason::FailedPrecondition => Error::failed_precondition("daemon not synced"),
            ErrorReason::Unavailable => Error::unavailable("imap unreachable"),
            ErrorReason::ResourceExhausted => Error::resource_exhausted("daily budget spent"),
            ErrorReason::DeadlineExceeded => Error::deadline_exceeded("search timed out"),
            ErrorReason::AlreadyExists => Error::already_exists("idempotency key reused"),
            ErrorReason::OutOfRange => Error::out_of_range("cursor past retention"),
            ErrorReason::InvalidArgument => Error::invalid_argument("empty query"),
            ErrorReason::Internal => Error::internal("unexpected null in ranker"),
        }
    }

    /// An independent (hardcoded) oracle for the reason→code contract, so this
    /// assertion is not just a tautology against `ErrorReason::code`.
    fn expected_code(reason: ErrorReason) -> Code {
        match reason {
            ErrorReason::Unauthenticated => Code::Unauthenticated,
            ErrorReason::PermissionDenied => Code::PermissionDenied,
            ErrorReason::NotFound => Code::NotFound,
            ErrorReason::FailedPrecondition => Code::FailedPrecondition,
            ErrorReason::Unavailable => Code::Unavailable,
            ErrorReason::ResourceExhausted => Code::ResourceExhausted,
            ErrorReason::DeadlineExceeded => Code::DeadlineExceeded,
            ErrorReason::AlreadyExists => Code::AlreadyExists,
            ErrorReason::OutOfRange => Code::OutOfRange,
            ErrorReason::InvalidArgument => Code::InvalidArgument,
            ErrorReason::Internal => Code::Internal,
        }
    }

    #[test]
    fn every_variant_maps_to_expected_code_and_reason() {
        for reason in ErrorReason::ALL {
            let err = sample(reason);
            assert_eq!(err.reason(), reason);
            assert_eq!(err.code(), expected_code(reason), "code for {reason:?}");

            let status: Status = err.into();
            assert_eq!(
                status.code(),
                expected_code(reason),
                "status code for {reason:?}"
            );

            // Clients branch on the ErrorInfo reason + domain, never the message.
            let details = status.get_error_details();
            assert!(
                details.error_info().is_some(),
                "missing ErrorInfo for {reason:?}"
            );
            let info = details.error_info().expect("error info present");
            assert_eq!(info.reason, reason.as_str(), "wire reason for {reason:?}");
            assert_eq!(info.domain, ERROR_DOMAIN);
        }
    }

    #[test]
    fn reason_wire_strings_are_stable() {
        assert_eq!(ErrorReason::Unauthenticated.as_str(), "UNAUTHENTICATED");
        assert_eq!(ErrorReason::PermissionDenied.as_str(), "PERMISSION_DENIED");
        assert_eq!(ErrorReason::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(
            ErrorReason::FailedPrecondition.as_str(),
            "FAILED_PRECONDITION"
        );
        assert_eq!(ErrorReason::Unavailable.as_str(), "UNAVAILABLE");
        assert_eq!(
            ErrorReason::ResourceExhausted.as_str(),
            "RESOURCE_EXHAUSTED"
        );
        assert_eq!(ErrorReason::DeadlineExceeded.as_str(), "DEADLINE_EXCEEDED");
        assert_eq!(ErrorReason::AlreadyExists.as_str(), "ALREADY_EXISTS");
        assert_eq!(ErrorReason::OutOfRange.as_str(), "OUT_OF_RANGE");
        assert_eq!(ErrorReason::InvalidArgument.as_str(), "INVALID_ARGUMENT");
        assert_eq!(ErrorReason::Internal.as_str(), "INTERNAL");
    }

    #[test]
    fn all_reasons_are_unique_and_have_distinct_wire_strings() {
        // Guards against a duplicate/typo in ALL or the wire mapping. If a new
        // reason is added to ALL, its wire string must also be distinct.
        let mut wire: Vec<&str> = ErrorReason::ALL.iter().map(|r| r.as_str()).collect();
        let total = wire.len();
        wire.sort_unstable();
        wire.dedup();
        assert_eq!(wire.len(), total, "duplicate reason wire strings in ALL");
    }

    #[test]
    fn internal_message_is_not_leaked_to_clients() {
        let err = Error::internal("secret: db password = hunter2");
        let status: Status = err.into();
        assert_eq!(status.code(), Code::Internal);
        assert_eq!(status.message(), "internal error");
        assert!(
            !status.message().contains("hunter2"),
            "internal detail must not cross the boundary"
        );
        // The reason is still branchable.
        let details = status.get_error_details();
        let info = details.error_info().expect("error info present");
        assert_eq!(info.reason, "INTERNAL");
    }

    #[test]
    fn non_internal_message_is_preserved() {
        let status: Status = Error::not_found("message 42").into();
        assert!(status.message().contains("message 42"));
    }

    #[test]
    fn bad_config_value_maps_to_invalid_argument() {
        let cfg_err = Config::from_toml_str("[grpc]\nauth = \"bogus\"\n")
            .expect_err("bad config should error");
        let err = Error::from(cfg_err);
        assert_eq!(err.reason(), ErrorReason::InvalidArgument);
        let status: Status = err.into();
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn missing_config_file_maps_to_failed_precondition() {
        let cfg_err =
            Config::load("/nonexistent/rmail/absent.toml").expect_err("missing file should error");
        let err = Error::from(cfg_err);
        assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    }
}
