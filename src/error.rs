use std::fmt;

use thiserror::Error;

use crate::{Lease, Proxy};

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
}

/// Why proxy-wide certified shutdown did not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ShutdownErrorKind {
    /// Tracked work remained at the caller's deadline.
    DeadlineExceeded,
    /// The proxy runtime stopped before certified shutdown completed.
    RuntimeStopped,
}

/// A failed proxy-wide shutdown that retains the stopping [`Proxy`].
pub struct ShutdownError {
    pub(crate) kind: ShutdownErrorKind,
    pub(crate) proxy: Proxy,
}

impl ShutdownError {
    /// Return the reason shutdown was not certified.
    pub const fn kind(&self) -> ShutdownErrorKind {
        self.kind
    }

    /// Recover the stopping proxy so shutdown can be retried when its runtime
    /// remains available. New leases stay refused after the first attempt.
    pub fn into_proxy(self) -> Proxy {
        self.proxy
    }
}

impl fmt::Debug for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ShutdownErrorKind::DeadlineExceeded => {
                formatter.write_str("proxy shutdown exceeded its deadline")
            }
            ShutdownErrorKind::RuntimeStopped => {
                formatter.write_str("proxy runtime stopped before certified shutdown")
            }
        }
    }
}

impl std::error::Error for ShutdownError {}

/// Invalid immutable policy construction.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum PolicyError {
    /// A hostname pattern was malformed or ambiguous.
    #[error("invalid hostname pattern: {0}")]
    InvalidHostPattern(String),
    /// TCP destination port zero cannot be connected.
    #[error("destination port must be greater than zero")]
    InvalidPort,
    /// A lease must admit at least one possible connection.
    #[error("connection limit must be greater than zero")]
    ZeroConnectionLimit,
    /// A connection limit must fit the asynchronous runtime's semaphore.
    #[error("connection limit is too large for the runtime")]
    ConnectionLimitTooLarge,
    /// Connection-attempt rate and burst limits must both be positive.
    #[error("connection attempt rate and burst must be greater than zero")]
    ZeroConnectionAttemptRate,
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
    /// The identity cannot unambiguously identify an accepted TCP connection.
    #[error("peer identity cannot unambiguously identify an accepted connection")]
    InvalidIdentity,
    /// Another open or revoking lease owns the identity.
    #[error("peer identity is already attached or still revoking")]
    IdentityInUse,
    /// The proxy cannot assign another unique process-local lease sequence.
    #[error("proxy lease sequence is exhausted")]
    LeaseIdExhausted,
    /// Proxy-wide shutdown has begun, so no new lease may be installed.
    #[error("proxy shutdown has begun")]
    ProxyStopping,
    /// The listener could not be inspected before installing a replacement
    /// policy for this identity.
    #[error("proxy listener is temporarily unavailable")]
    ListenerUnavailable,
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
    /// The listener could not be drained, so identity cleanup was not
    /// certified.
    ListenerUnavailable,
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
            CloseErrorKind::ListenerUnavailable => {
                formatter.write_str("proxy listener could not be drained for certified shutdown")
            }
        }
    }
}

impl std::error::Error for CloseError {}
