#![doc = include_str!("../README.md")]

mod config;
mod connect;
mod diagnostic;
mod error;
mod identity;
mod policy;
mod proxy;
mod rate;
mod resolver;
mod tls;
#[cfg(test)]
mod tls_tests;
mod upstream;
mod usage;

// Keep the advanced examples executable after moving them out of the README.
#[cfg(doctest)]
#[doc = include_str!("../docs/configuration.md")]
mod configuration_examples {}

pub use config::ProxyConfig;
pub use diagnostic::{DenialReason, DiagnosticEvent};
pub use error::{
    AttachError, CloseError, CloseErrorKind, PolicyError, ProxyError, ShutdownError,
    ShutdownErrorKind,
};
pub use identity::{Endpoint, PeerIdentity};
pub use policy::{EchPolicy, Policy, PolicyBuilder, TlsAuthority};
pub use proxy::{Lease, Proxy};
pub use usage::{FinalUsage, Usage};
