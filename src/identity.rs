use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// Identity evidence derived by the trusted host boundary.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// The source address the proxy listener observes, enforced by the trusted
    /// namespace, routing, or NAT boundary. This may be a host-translated
    /// address rather than the address configured inside the guest. A proxy
    /// treats an IPv4-mapped IPv6 spelling as the equivalent IPv4 address at
    /// both attachment and socket acceptance.
    SourceIp(IpAddr),
}

impl PeerIdentity {
    pub(crate) fn canonical(self) -> Self {
        match self {
            Self::SourceIp(IpAddr::V6(address)) => address
                .to_ipv4_mapped()
                .map_or(Self::SourceIp(IpAddr::V6(address)), |address| {
                    Self::SourceIp(IpAddr::V4(address))
                }),
            identity => identity,
        }
    }

    pub(crate) const fn is_attachable(&self) -> bool {
        match self {
            Self::SourceIp(IpAddr::V4(address)) => {
                !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
            }
            Self::SourceIp(IpAddr::V6(address)) => {
                !address.is_unspecified() && !address.is_multicast()
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn canonicalizes_only_the_ipv4_mapped_transport_spelling() {
        let ipv4 = Ipv4Addr::new(192, 0, 2, 1);
        let compatible = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0xc000, 0x0201);
        assert_eq!(
            PeerIdentity::SourceIp(IpAddr::V6(ipv4.to_ipv6_mapped())).canonical(),
            PeerIdentity::SourceIp(IpAddr::V4(ipv4))
        );
        assert_eq!(
            PeerIdentity::SourceIp(IpAddr::V6(compatible)).canonical(),
            PeerIdentity::SourceIp(IpAddr::V6(compatible))
        );
    }
}
