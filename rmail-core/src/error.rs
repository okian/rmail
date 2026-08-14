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

/// `ErrorInfo` metadata key carrying the oldest still-available log position on
/// an [`ErrorReason::OutOfRange`] resume gap.
///
/// This is an **event id**, not a cursor — reads are strictly-after, so passing
/// it back as a cursor would skip the very event it names. Use
/// [`RESUME_FROM_KEY`] for that.
///
/// Named as a constant because it is a wire contract: a client branches on it.
pub const OLDEST_SEQ_KEY: &str = "oldest_seq";

/// `ErrorInfo` metadata key carrying the **cursor** to resume from after an
/// [`ErrorReason::OutOfRange`] resume gap.
///
/// Exactly one less than [`OLDEST_SEQ_KEY`], and separate from it because the
/// difference between an id and a cursor is one silently dropped event.
pub const RESUME_FROM_KEY: &str = "resume_from";

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
    /// A concurrent attempt owns the operation — an idempotency key whose
    /// first attempt is still in flight (or died holding the fence).
    ///
    /// Distinct from [`ErrorReason::AlreadyExists`] on purpose: "you sent a
    /// different payload under a key you already used" is a client bug the
    /// caller must fix, while "this exact request is already running" resolves
    /// on its own. Collapsing them would make the one branch a retrying client
    /// needs indistinguishable from the one it must never retry.
    Aborted,
    /// A cursor is past retention (e.g. `WatchEvents` resume gap); carries the
    /// oldest still-available position so the client can resync.
    OutOfRange,
    /// Client input was malformed.
    InvalidArgument,
    /// The daemon ended the operation before it completed — a shut-down
    /// server, or a stream whose work was cancelled underneath it.
    ///
    /// Load-bearing for **streaming** RPCs. A server-streamed call that simply
    /// stops yielding frames terminates `OK`, which a client cannot tell apart
    /// from a complete answer: it keeps half an `AskMailbox` reply, sees
    /// success, and exits 0. A cancelled stream therefore ends with this
    /// reason rather than silently.
    Cancelled,
    /// An unexpected internal error (the detail stays server-side; the boundary
    /// returns a generic message).
    Internal,
}

impl ErrorReason {
    /// Every reason, for exhaustive iteration in tests and tooling.
    pub const ALL: [ErrorReason; 13] = [
        ErrorReason::Unauthenticated,
        ErrorReason::PermissionDenied,
        ErrorReason::NotFound,
        ErrorReason::FailedPrecondition,
        ErrorReason::Unavailable,
        ErrorReason::ResourceExhausted,
        ErrorReason::DeadlineExceeded,
        ErrorReason::AlreadyExists,
        ErrorReason::Aborted,
        ErrorReason::OutOfRange,
        ErrorReason::InvalidArgument,
        ErrorReason::Cancelled,
        ErrorReason::Internal,
    ];

    /// The reason whose wire string is `wire`, if any.
    ///
    /// The inverse of [`ErrorReason::as_str`], and the reason that function's
    /// output is a closed vocabulary: [`Error::from_status`] uses this to
    /// recover a domain error from a `Status` that crossed a service seam, and
    /// an unknown string there must degrade rather than guess.
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == wire)
    }

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
            ErrorReason::Aborted => "ABORTED",
            ErrorReason::OutOfRange => "OUT_OF_RANGE",
            ErrorReason::InvalidArgument => "INVALID_ARGUMENT",
            ErrorReason::Cancelled => "CANCELLED",
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
            ErrorReason::Aborted => Code::Aborted,
            ErrorReason::OutOfRange => Code::OutOfRange,
            ErrorReason::InvalidArgument => Code::InvalidArgument,
            ErrorReason::Cancelled => Code::Cancelled,
            ErrorReason::Internal => Code::Internal,
        }
    }

    /// The reason a bare gRPC [`Code`] implies, for a `Status` that carries no
    /// rmail `ErrorInfo` at all.
    ///
    /// Only [`Error::from_status`] should need this: a `Status` minted by
    /// tonic itself (a decode failure, an unimplemented method, a transport
    /// error) has a meaningful code and nothing else, and throwing that code
    /// away is precisely the flattening `from_status` exists to stop.
    #[must_use]
    pub const fn from_code(code: Code) -> Self {
        match code {
            Code::Unauthenticated => ErrorReason::Unauthenticated,
            Code::PermissionDenied => ErrorReason::PermissionDenied,
            Code::NotFound => ErrorReason::NotFound,
            Code::FailedPrecondition => ErrorReason::FailedPrecondition,
            Code::Unavailable => ErrorReason::Unavailable,
            Code::ResourceExhausted => ErrorReason::ResourceExhausted,
            Code::DeadlineExceeded => ErrorReason::DeadlineExceeded,
            Code::AlreadyExists => ErrorReason::AlreadyExists,
            Code::Aborted => ErrorReason::Aborted,
            Code::OutOfRange => ErrorReason::OutOfRange,
            Code::InvalidArgument => ErrorReason::InvalidArgument,
            Code::Cancelled => ErrorReason::Cancelled,
            // Ok/Unknown/Unimplemented/DataLoss and anything tonic adds later
            // have no domain counterpart; `Internal` is the honest floor.
            _ => ErrorReason::Internal,
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

    /// A concurrent attempt owns the operation — see [`ErrorReason::Aborted`].
    #[error("aborted: {0}")]
    Aborted(String),

    /// A cursor is past retention (e.g. a `WatchEvents` resume gap).
    ///
    /// Carries the oldest still-available position when there is one, so a
    /// client can resync from a real cursor instead of guessing or starting
    /// over. It reaches the wire as `ErrorInfo` metadata, never as message
    /// text — see [`Error::metadata`].
    #[error("out of range: {message}")]
    OutOfRange {
        /// Client-safe description of the gap.
        message: String,
        /// The oldest position still retained, if the log has one.
        oldest_seq: Option<i64>,
    },

    /// Client input was malformed.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The daemon ended the operation before it completed — see
    /// [`ErrorReason::Cancelled`].
    #[error("cancelled: {0}")]
    Cancelled(String),

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
    /// Construct an [`Error::Aborted`].
    pub fn aborted(message: impl Into<String>) -> Self {
        Self::Aborted(message.into())
    }
    /// Construct an [`Error::Cancelled`].
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::Cancelled(message.into())
    }
    /// Construct an [`Error::OutOfRange`].
    pub fn out_of_range(message: impl Into<String>) -> Self {
        Self::OutOfRange {
            message: message.into(),
            oldest_seq: None,
        }
    }

    /// Construct an [`Error::OutOfRange`] that tells the client where to resume.
    ///
    /// A resume gap without a cursor leaves a client with nothing to do but
    /// start from the beginning; with one it can resync exactly the span it
    /// missed.
    pub fn resume_gap(message: impl Into<String>, oldest_seq: i64) -> Self {
        Self::OutOfRange {
            message: message.into(),
            oldest_seq: Some(oldest_seq),
        }
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
            Error::Aborted(_) => ErrorReason::Aborted,
            Error::OutOfRange { .. } => ErrorReason::OutOfRange,
            Error::InvalidArgument(_) => ErrorReason::InvalidArgument,
            Error::Cancelled(_) => ErrorReason::Cancelled,
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
    /// This is how a client gets actionable detail without parsing message
    /// text — the one thing the error contract promises will never be stable.
    /// Variants populate it as the contract grows; retry and budget hints for
    /// [`ErrorReason::ResourceExhausted`] are the next candidates.
    #[must_use]
    fn metadata(&self) -> HashMap<String, String> {
        match self {
            Error::OutOfRange {
                oldest_seq: Some(seq),
                ..
            } => HashMap::from([
                (OLDEST_SEQ_KEY.to_owned(), seq.to_string()),
                // The cursor whose strictly-after read begins with `seq`.
                // Saturating because a caller could name position 0, though the
                // log never assigns it.
                (RESUME_FROM_KEY.to_owned(), (seq - 1).max(0).to_string()),
            ]),
            _ => HashMap::new(),
        }
    }

    /// Convert into a [`tonic::Status`] carrying `google.rpc.ErrorInfo`.
    #[must_use]
    pub fn into_status(self) -> Status {
        Status::from(self)
    }

    /// Recover a domain [`Error`] from a [`Status`], preserving its reason.
    ///
    /// # Why this exists
    ///
    /// The daemon has seams where one component's gRPC-shaped result is fed
    /// back into a `rmail-core` trait that speaks [`Error`] — `AskMailbox`'s
    /// retriever and `Evaluate`'s ranked-search adapter both call the search
    /// pipeline, which returns [`Status`] because every *other* caller of it
    /// is a tonic handler. Those seams used to wrap the status message in
    /// [`Error::Internal`], which is lossy twice over: the code became
    /// `INTERNAL`, and the [`From<Error>`] impl below then scrubbed the
    /// message — so a caller naming an account that does not exist got
    /// `INTERNAL`/`"internal error"`, indistinguishable from a daemon bug and
    /// unactionable by a client that (correctly) branches on `reason`.
    ///
    /// Round-tripping through this function is lossless for every reason the
    /// contract defines, so a `NOT_FOUND` raised deep in retrieval is still a
    /// `NOT_FOUND` when `AskMailbox` re-raises it.
    ///
    /// # Trust
    ///
    /// The `ErrorInfo` is only honoured when its `domain` is
    /// [`ERROR_DOMAIN`] — a status from some other server's interceptor may
    /// carry an `ErrorInfo` of its own with a colliding `reason` string, and
    /// adopting that would let a foreign vocabulary drive rmail's own
    /// branching. Anything else falls back to
    /// [`ErrorReason::from_code`], which is still strictly better than
    /// flattening to `INTERNAL`.
    ///
    /// `Internal` statuses keep the boundary's generic message: the detail was
    /// already scrubbed on the way out and there is nothing to recover.
    #[must_use]
    pub fn from_status(status: &Status) -> Self {
        let details = status.get_error_details();
        let reason = details
            .error_info()
            .filter(|info| info.domain == ERROR_DOMAIN)
            .and_then(|info| ErrorReason::from_wire(&info.reason))
            .unwrap_or_else(|| ErrorReason::from_code(status.code()));

        let message = status.message().to_owned();
        match reason {
            ErrorReason::Unauthenticated => Self::Unauthenticated(message),
            ErrorReason::PermissionDenied => Self::PermissionDenied(message),
            ErrorReason::NotFound => Self::NotFound(message),
            ErrorReason::FailedPrecondition => Self::FailedPrecondition(message),
            ErrorReason::Unavailable => Self::Unavailable(message),
            ErrorReason::ResourceExhausted => Self::ResourceExhausted(message),
            ErrorReason::DeadlineExceeded => Self::DeadlineExceeded(message),
            ErrorReason::AlreadyExists => Self::AlreadyExists(message),
            ErrorReason::Aborted => Self::Aborted(message),
            // The resume cursor rides in metadata, not in the message, so it
            // survives the round trip too.
            ErrorReason::OutOfRange => Self::OutOfRange {
                message,
                oldest_seq: details
                    .error_info()
                    .and_then(|info| info.metadata.get(OLDEST_SEQ_KEY).cloned())
                    .and_then(|seq| seq.parse().ok()),
            },
            ErrorReason::InvalidArgument => Self::InvalidArgument(message),
            ErrorReason::Cancelled => Self::Cancelled(message),
            ErrorReason::Internal => Self::Internal(message),
        }
    }

    /// This error with `context` prefixed onto its message, keeping its
    /// reason.
    ///
    /// The safe form of `Error::internal(format!("{context}: {err}"))`, which
    /// is how context normally gets lost: prefixing must not re-code the
    /// error, because the code is the only part a client is allowed to branch
    /// on.
    ///
    /// The prefix goes on the *inner* message, not on `Display`'s output — the
    /// latter already carries the variant's own name, so `"ask retrieval: "`
    /// applied to it would read `"unauthenticated: ask retrieval:
    /// unauthenticated: ..."` once the boundary re-rendered it.
    #[must_use]
    pub fn context(self, context: &str) -> Self {
        let prefixed = |message: String| format!("{context}: {message}");
        match self {
            Self::Unauthenticated(m) => Self::Unauthenticated(prefixed(m)),
            Self::PermissionDenied(m) => Self::PermissionDenied(prefixed(m)),
            Self::NotFound(m) => Self::NotFound(prefixed(m)),
            Self::FailedPrecondition(m) => Self::FailedPrecondition(prefixed(m)),
            Self::Unavailable(m) => Self::Unavailable(prefixed(m)),
            Self::ResourceExhausted(m) => Self::ResourceExhausted(prefixed(m)),
            Self::DeadlineExceeded(m) => Self::DeadlineExceeded(prefixed(m)),
            Self::AlreadyExists(m) => Self::AlreadyExists(prefixed(m)),
            Self::Aborted(m) => Self::Aborted(prefixed(m)),
            Self::OutOfRange {
                message,
                oldest_seq,
            } => Self::OutOfRange {
                message: prefixed(message),
                oldest_seq,
            },
            Self::InvalidArgument(m) => Self::InvalidArgument(prefixed(m)),
            Self::Cancelled(m) => Self::Cancelled(prefixed(m)),
            Self::Internal(m) => Self::Internal(prefixed(m)),
        }
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
            ErrorReason::Aborted => Error::aborted("that idempotency key is still in flight"),
            ErrorReason::OutOfRange => Error::out_of_range("cursor past retention"),
            ErrorReason::InvalidArgument => Error::invalid_argument("empty query"),
            ErrorReason::Cancelled => Error::cancelled("the daemon is shutting down"),
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
            ErrorReason::Aborted => Code::Aborted,
            ErrorReason::OutOfRange => Code::OutOfRange,
            ErrorReason::InvalidArgument => Code::InvalidArgument,
            ErrorReason::Cancelled => Code::Cancelled,
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
    fn only_a_resume_gap_carries_metadata() {
        // A future variant leaking a field onto every error would put
        // implementation detail on the wire for all of them.
        for reason in ErrorReason::ALL {
            assert!(
                sample(reason).metadata().is_empty(),
                "{reason:?} should carry no metadata"
            );
        }
    }

    #[test]
    fn a_resume_gap_carries_both_the_floor_and_the_cursor() {
        // They differ by one, and the difference is a silently dropped event:
        // reads are strictly-after, so resuming *at* the oldest id skips it.
        let status = Status::from(Error::resume_gap("cursor past retention", 16));
        assert_eq!(status.code(), Code::OutOfRange);
        let details = status.get_error_details();
        let info = details.error_info().expect("ErrorInfo attached");
        assert_eq!(info.metadata.get(OLDEST_SEQ_KEY), Some(&"16".to_owned()));
        assert_eq!(info.metadata.get(RESUME_FROM_KEY), Some(&"15".to_owned()));
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
        assert_eq!(ErrorReason::Aborted.as_str(), "ABORTED");
        assert_eq!(ErrorReason::OutOfRange.as_str(), "OUT_OF_RANGE");
        assert_eq!(ErrorReason::InvalidArgument.as_str(), "INVALID_ARGUMENT");
        assert_eq!(ErrorReason::Cancelled.as_str(), "CANCELLED");
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
    fn a_status_round_trips_back_to_its_reason() {
        // The seam this exists for: a `Status` produced by one component,
        // handed back to a `rmail-core` trait that speaks `Error`. Every
        // reason must survive it, or a client's branch silently changes.
        for reason in ErrorReason::ALL {
            let status: Status = sample(reason).into();
            let recovered = Error::from_status(&status);
            assert_eq!(recovered.reason(), reason, "round trip for {reason:?}");
            let again: Status = recovered.into();
            assert_eq!(again.code(), reason.code(), "code for {reason:?}");
        }
    }

    #[test]
    fn a_recovered_error_keeps_its_client_safe_message() {
        let status: Status = Error::not_found("account 7").into();
        let recovered = Error::from_status(&status);
        assert!(recovered.to_string().contains("account 7"));
    }

    #[test]
    fn a_recovered_resume_gap_keeps_its_cursor() {
        let status: Status = Error::resume_gap("cursor past retention", 16).into();
        let recovered = Error::from_status(&status);
        assert_eq!(recovered.reason(), ErrorReason::OutOfRange);
        let oldest_seq = match recovered {
            Error::OutOfRange { oldest_seq, .. } => oldest_seq,
            other => unreachable!("expected an OutOfRange, got {other:?}"),
        };
        assert_eq!(oldest_seq, Some(16));
    }

    #[test]
    fn a_foreign_status_falls_back_to_its_code() {
        // No ErrorInfo at all — a tonic-minted status (decode failure,
        // unimplemented method). The code is all there is, and it is still far
        // better than flattening to INTERNAL.
        let status = Status::new(Code::NotFound, "no such method");
        assert_eq!(Error::from_status(&status).reason(), ErrorReason::NotFound);

        let status = Status::new(Code::Unimplemented, "not built");
        assert_eq!(Error::from_status(&status).reason(), ErrorReason::Internal);
    }

    #[test]
    fn a_foreign_error_info_domain_is_not_adopted() {
        // Another server's ErrorInfo may use the same reason strings for
        // entirely different semantics; only rmail's own domain is trusted.
        let mut details = ErrorDetails::new();
        details.set_error_info("NOT_FOUND", "example.com", HashMap::new());
        let status = Status::with_error_details(Code::Internal, "boom", details);
        assert_eq!(Error::from_status(&status).reason(), ErrorReason::Internal);
    }

    #[test]
    fn an_unknown_reason_string_falls_back_to_the_code() {
        // A newer daemon adding a reason this build has never heard of must
        // degrade to the code rather than guess.
        let mut details = ErrorDetails::new();
        details.set_error_info("TEAPOT", ERROR_DOMAIN, HashMap::new());
        let status = Status::with_error_details(Code::InvalidArgument, "nope", details);
        assert_eq!(
            Error::from_status(&status).reason(),
            ErrorReason::InvalidArgument
        );
    }

    #[test]
    fn context_prefixes_without_re_coding() {
        for reason in ErrorReason::ALL {
            let original = sample(reason).to_string();
            let err = sample(reason).context("ask retrieval");
            assert_eq!(err.reason(), reason, "context re-coded {reason:?}");
            let rendered = err.to_string();
            assert!(
                rendered.contains("ask retrieval: "),
                "context missing for {reason:?}: {rendered}"
            );
            // Prefixed once, on the inner message — not wrapped around a
            // Display that already names the variant.
            assert_eq!(
                rendered
                    .matches(&original[..original.find(':').unwrap_or(0)])
                    .count(),
                1,
                "the variant name was repeated for {reason:?}: {rendered}"
            );
        }
    }

    #[test]
    fn context_on_an_internal_still_does_not_reach_the_client() {
        let status: Status = Error::internal("db password = hunter2")
            .context("ask retrieval")
            .into();
        assert_eq!(status.message(), "internal error");
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
