//! The exit-code table (task 42).
//!
//! # Why an enum and not `1`
//!
//! An exit code is the only thing a shell can branch on without parsing
//! output, and it is part of the same contract `--format json` is: a CI job
//! that retries on "the daemon was not up" but fails the build on "this token
//! cannot do that" needs those two to be different numbers, and needs them to
//! stay different. A blanket `1` forces every caller to grep stderr, which is
//! neither stable nor localizable.
//!
//! # The table
//!
//! | code | name | when |
//! |---|---|---|
//! | 0 | success | the command did what it said |
//! | 1 | failure | a local failure with no better classification |
//! | 2 | usage | bad arguments — `clap`'s own code for this, kept |
//! | 3 | unavailable | rmaild is not running, or the socket refused |
//! | 4 | unauthenticated | no credential, or one the daemon rejected |
//! | 5 | permission denied | authenticated, but the scope does not cover it |
//! | 6 | not found | the account, message, tag, hook … does not exist |
//! | 7 | already exists | a create that collided |
//! | 8 | invalid argument | the daemon rejected the request's contents |
//! | 9 | failed precondition | the system is not in a state where this works |
//! | 10 | deadline exceeded | `--deadline` (or the server's own) expired |
//! | 11 | resource exhausted | a quota, a rate limit, an AI budget cap |
//! | 12 | unimplemented | the daemon does not serve this, or this build cannot render it |
//! | 13 | cancelled | interrupted, locally or by the server |
//! | 14 | internal | the daemon failed, or answered something it should not have |
//!
//! Nothing here uses 126, 127 or anything ≥ 128: those belong to the shell
//! (not executable, not found, killed by signal N) and reusing one would make
//! `mail`'s own failures indistinguishable from the shell's.
//!
//! # Derived from the error, not passed around
//!
//! [`ExitCode::of`] walks an `anyhow` chain looking for a `tonic::Status` and
//! maps its code. Every RPC call site in this crate already attaches context
//! with `?`, which preserves the `Status` as a source — so classification
//! works for all ninety-odd verbs without one of them being edited, and a verb
//! added tomorrow is classified the same way with no new code. The alternative
//! — every command returning its own code — is exactly the sort of table that
//! is correct on the day it is written and wrong a month later.

use tonic::Code;

/// What `mail` exits with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum ExitCode {
    /// The command did what it said.
    Success = 0,
    /// A local failure with no better classification: a file that could not be
    /// read, an editor that exited non-zero, a response that made no sense.
    Failure = 1,
    /// Bad arguments. `clap` exits with this itself before `main` runs; the
    /// variant exists so the table is complete and so a hand-written usage
    /// error agrees with it.
    Usage = 2,
    /// The daemon is not running, or the channel could not be established.
    Unavailable = 3,
    /// No credential was presented, or the daemon rejected the one that was.
    Unauthenticated = 4,
    /// Authenticated, and the principal's scopes do not cover this method.
    PermissionDenied = 5,
    /// The thing addressed does not exist.
    NotFound = 6,
    /// A create collided with something already there.
    AlreadyExists = 7,
    /// The daemon rejected the request's contents.
    InvalidArgument = 8,
    /// The system is not in a state where this can work — including "the
    /// daemon is not started and this build does not start it for you".
    FailedPrecondition = 9,
    /// A deadline expired, ours or the server's.
    DeadlineExceeded = 10,
    /// A quota, rate limit or spend cap refused the work.
    ResourceExhausted = 11,
    /// The daemon does not serve this method, or this build has no structured
    /// rendering for the verb that was asked for one.
    Unimplemented = 12,
    /// The call was cancelled — an interrupt, or a server that gave up.
    Cancelled = 13,
    /// The daemon failed, or answered something it should not have.
    Internal = 14,
}

impl ExitCode {
    /// Every variant, for the tests that pin the table.
    ///
    /// Test-only by construction — the binary never enumerates its own exit
    /// codes — but declared here rather than in `tests.rs` so the list lives
    /// next to the enum it mirrors and a new variant is one edit away from
    /// being covered.
    #[cfg(test)]
    pub(crate) const ALL: &'static [ExitCode] = &[
        ExitCode::Success,
        ExitCode::Failure,
        ExitCode::Usage,
        ExitCode::Unavailable,
        ExitCode::Unauthenticated,
        ExitCode::PermissionDenied,
        ExitCode::NotFound,
        ExitCode::AlreadyExists,
        ExitCode::InvalidArgument,
        ExitCode::FailedPrecondition,
        ExitCode::DeadlineExceeded,
        ExitCode::ResourceExhausted,
        ExitCode::Unimplemented,
        ExitCode::Cancelled,
        ExitCode::Internal,
    ];

    /// The number the process exits with.
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    /// The name used in documentation and in `--format json` error output.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            ExitCode::Success => "success",
            ExitCode::Failure => "failure",
            ExitCode::Usage => "usage",
            ExitCode::Unavailable => "unavailable",
            ExitCode::Unauthenticated => "unauthenticated",
            ExitCode::PermissionDenied => "permission_denied",
            ExitCode::NotFound => "not_found",
            ExitCode::AlreadyExists => "already_exists",
            ExitCode::InvalidArgument => "invalid_argument",
            ExitCode::FailedPrecondition => "failed_precondition",
            ExitCode::DeadlineExceeded => "deadline_exceeded",
            ExitCode::ResourceExhausted => "resource_exhausted",
            ExitCode::Unimplemented => "unimplemented",
            ExitCode::Cancelled => "cancelled",
            ExitCode::Internal => "internal",
        }
    }

    /// The code a gRPC status maps to.
    ///
    /// Total over `tonic::Code` rather than a `_` arm, so a code added by a
    /// future tonic fails the build here instead of silently becoming
    /// `Internal`.
    pub(crate) const fn of_status(code: Code) -> Self {
        match code {
            Code::Ok => ExitCode::Success,
            Code::Cancelled => ExitCode::Cancelled,
            Code::InvalidArgument => ExitCode::InvalidArgument,
            Code::DeadlineExceeded => ExitCode::DeadlineExceeded,
            Code::NotFound => ExitCode::NotFound,
            Code::AlreadyExists => ExitCode::AlreadyExists,
            Code::PermissionDenied => ExitCode::PermissionDenied,
            Code::ResourceExhausted => ExitCode::ResourceExhausted,
            Code::FailedPrecondition => ExitCode::FailedPrecondition,
            Code::Unimplemented => ExitCode::Unimplemented,
            Code::Unavailable => ExitCode::Unavailable,
            Code::Unauthenticated => ExitCode::Unauthenticated,
            // `Aborted` is a concurrency conflict, `OutOfRange` a bad
            // argument the server could only detect late, `DataLoss` and
            // `Unknown` are failures with no client-side remedy. None of the
            // four has a distinct *action* attached to it from a shell, so
            // they share the codes of the situations they resemble rather
            // than inflating the table with numbers nobody branches on.
            Code::Aborted | Code::Unknown | Code::DataLoss | Code::Internal => ExitCode::Internal,
            Code::OutOfRange => ExitCode::InvalidArgument,
        }
    }

    /// Classify a failure from the whole `anyhow` chain.
    ///
    /// The chain matters: every RPC call site in this crate writes
    /// `.context("… RPC failed")?`, which makes the `tonic::Status` a
    /// *source* of the returned error rather than the error itself. Looking
    /// only at the outermost error would classify every RPC failure in the
    /// binary as [`ExitCode::Failure`].
    pub(crate) fn of(error: &anyhow::Error) -> Self {
        for cause in error.chain() {
            if let Some(status) = cause.downcast_ref::<tonic::Status>() {
                return Self::of_status(status.code());
            }
            // A channel that never connected: rmaild is not running, the
            // socket path is wrong, or the TCP endpoint refused. `tonic`'s
            // transport error deliberately hides its own source behind an
            // opaque type, so the *kind* of connection failure is not
            // recoverable here — but "could not talk to the daemon at all" is
            // exactly one situation from a script's point of view, and it is
            // the one worth retrying.
            if cause.downcast_ref::<tonic::transport::Error>().is_some() {
                return ExitCode::Unavailable;
            }
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                return match io.kind() {
                    std::io::ErrorKind::NotFound => ExitCode::NotFound,
                    std::io::ErrorKind::PermissionDenied => ExitCode::PermissionDenied,
                    std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::AddrNotAvailable => ExitCode::Unavailable,
                    std::io::ErrorKind::TimedOut => ExitCode::DeadlineExceeded,
                    std::io::ErrorKind::AlreadyExists => ExitCode::AlreadyExists,
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
                        ExitCode::InvalidArgument
                    }
                    _ => ExitCode::Failure,
                };
            }
            if let Some(classified) = cause.downcast_ref::<Classified>() {
                return classified.code;
            }
        }
        ExitCode::Failure
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        std::process::ExitCode::from(code.code())
    }
}

/// A locally-detected failure that already knows its exit code.
///
/// The way a command says "this is a `FAILED_PRECONDITION`, not a generic
/// failure" without inventing a `tonic::Status` it never received from a
/// server. Synthesizing a `Status` would have been fewer lines and would have
/// read, in every log and every error message, as though the daemon had
/// answered — which is the distinction `rmaild::mcp::McpError::Timeout`'s own
/// docs make, for the same reason.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct Classified {
    code: ExitCode,
    message: String,
}

impl Classified {
    /// A failure carrying its own exit code.
    ///
    /// Named `new` and returning `anyhow::Error` rather than `Self` because
    /// there is no use for a bare `Classified`: every call site is a
    /// `return Err(...)` or a `?`, and handing back the concrete type would
    /// make each one write `.into()`.
    #[allow(
        clippy::new_ret_no_self,
        reason = "the only sensible product of this constructor is the boxed error"
    )]
    pub(crate) fn new(code: ExitCode, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            code,
            message: message.into(),
        })
    }
}

#[cfg(test)]
mod tests;
