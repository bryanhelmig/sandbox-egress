use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// Identity evidence derived by the trusted host boundary.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// The socket source address enforced by the guest namespace/NAT boundary.
    SourceIp(IpAddr),
}

/// The HTTP proxy endpoint to expose inside the guest.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint(SocketAddr);

impl Endpoint {
    pub(crate) const fn new(address: SocketAddr) -> Self {
        Self(address)
    }

    /// Return the underlying listener address.
    pub const fn socket_addr(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "http://{}", self.0)
    }
}
