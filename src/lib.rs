#![doc = include_str!("../README.md")]

mod config;
mod connect;
mod diagnostic;
mod error;
mod identity;
mod policy;
mod proxy;
mod tls;
#[cfg(test)]
mod tls_tests;
mod usage;

pub use config::ProxyConfig;
pub use diagnostic::{DenialReason, DiagnosticEvent};
pub use error::{AttachError, CloseError, CloseErrorKind, PolicyError, ProxyError};
pub use identity::{Endpoint, PeerIdentity};
pub use policy::{EchPolicy, HostPattern, Policy, PolicyBuilder, TlsAuthority};
pub use proxy::{Lease, Proxy};
pub use usage::{FinalUsage, Usage};
