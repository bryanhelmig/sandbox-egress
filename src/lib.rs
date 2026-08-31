#![doc = include_str!("../README.md")]

mod config;
mod error;
mod identity;
mod policy;
mod proxy;
mod usage;

pub use config::ProxyConfig;
pub use error::{AttachError, CloseError, CloseErrorKind, PolicyError, ProxyError};
pub use identity::{Endpoint, PeerIdentity};
pub use policy::{HostPattern, Policy, PolicyBuilder};
pub use proxy::{Lease, Proxy};
pub use usage::{FinalUsage, Usage};
