//! Structured errors returned across the Platform Abstraction Layer boundary.

use std::time::Duration;

use loom_types::ProcessKey;
use thiserror::Error;

/// Structured platform error returned by all PAL traits.
#[derive(Debug, Error)]
pub enum PalError {
    /// The operation requires elevated (Administrator / root) privileges.
    #[error("administrator elevation is required for this operation")]
    ElevationRequired,

    /// The requested process no longer exists.
    #[error("process not found: {0}")]
    ProcessNotFound(ProcessKey),

    /// A process with the same PID exists, but its creation timestamp does not match
    /// (indicating OS PID reuse).
    #[error("process identity mismatch (PID reused): {0}")]
    ProcessIdentityMismatch(ProcessKey),

    /// A platform operation exceeded its deadline.
    #[error("platform operation timed out after {0:?}")]
    TimedOut(Duration),

    /// A slow-lane circuit breaker is currently open for `target`.
    #[error("circuit breaker open for '{target}' (retry after {retry_after_ms} ms)")]
    CircuitOpen {
        /// Identifier of the tripped device, drive, or subsystem.
        target: String,
        /// Remaining cooldown duration in milliseconds.
        retry_after_ms: u64,
    },

    /// The operation is unsupported on the active operating system or backend.
    #[error("operation unsupported on this platform: {0}")]
    Unsupported(&'static str),

    /// Caller supplied an invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Underlying OS API returned a structured error code.
    #[error("OS error {code}: {message}")]
    OsError {
        /// Platform error code (e.g., Win32 error, NTSTATUS, or POSIX errno).
        code: i32,
        /// Diagnostic error message.
        message: String,
    },

    /// Standard I/O error wrapper.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
