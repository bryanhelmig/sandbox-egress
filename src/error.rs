use std::fmt;

use thiserror::Error;

use crate::Lease;

/// Failure to create or start the shared proxy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProxyError {
    /// Listener or runtime initialization failed.
    #[error("proxy initialization failed: {0}")]
    Initialization(String),
    /// The runtime management thread stopped unexpectedly.
    #[error("proxy runtime stopped unexpectedly")]
    RuntimeStopped,
    /// Proxy-wide shutdown exceeded its deadline.
    #[error("proxy shutdown exceeded its deadline")]
    ShutdownTimeout,
}

/// Invalid immutable policy construction.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum PolicyError {
    /// A hostname pattern was malformed or ambiguous.
    #[error("invalid hostname pattern: {0}")]
    InvalidHostPattern(String),
    /// A lease must admit at least one possible connection.
    #[error("connection limit must be greater than zero")]
    ZeroConnectionLimit,
    /// A connection limit must fit the asynchronous runtime's semaphore.
    #[error("connection limit is too large for the runtime")]
    ConnectionLimitTooLarge,
    /// Deadlines must be positive.
    #[error("timeouts must be greater than zero")]
    ZeroTimeout,
    /// DNS must fit within the absolute handshake budget.
    #[error("DNS timeout cannot exceed handshake timeout")]
    DnsTimeoutExceedsHandshake,
    /// The operating system cannot represent the configured timeout deadline.
    #[error("timeout is too large to represent safely")]
    TimeoutTooLarge,
}

/// Failure to attach a run identity to the proxy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AttachError {
    /// Another open or revoking lease owns the identity.
    #[error("peer identity is already attached or still revoking")]
    IdentityInUse,
    /// The proxy cannot assign another unique process-local lease sequence.
    #[error("proxy lease sequence is exhausted")]
    LeaseIdExhausted,
    /// The proxy runtime stopped before attachment completed.
    #[error("proxy runtime stopped during attachment")]
    RuntimeStopped,
}

/// Why certified lease shutdown did not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CloseErrorKind {
    /// Tracked work remained at the caller's deadline.
    DeadlineExceeded,
    /// The proxy runtime stopped before certifying completion.
    RuntimeStopped,
}

/// A failed close that retains the still-owning [`Lease`].
pub struct CloseError {
    pub(crate) kind: CloseErrorKind,
    pub(crate) lease: Lease,
}

impl CloseError {
    /// Return the reason shutdown was not certified.
    pub const fn kind(&self) -> CloseErrorKind {
        self.kind
    }

    /// Recover ownership so shutdown can be retried. Dropping the returned
    /// lease still initiates best-effort cancellation.
    pub fn into_lease(self) -> Lease {
        self.lease
    }
}

impl fmt::Debug for CloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloseError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for CloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            CloseErrorKind::DeadlineExceeded => {
                formatter.write_str("lease shutdown exceeded its deadline")
            }
            CloseErrorKind::RuntimeStopped => {
                formatter.write_str("proxy runtime stopped before certifying lease shutdown")
            }
        }
    }
}

impl std::error::Error for CloseError {}
