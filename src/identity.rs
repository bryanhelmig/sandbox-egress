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
    /// both attachment and socket acceptance. Scoped IPv6 unicast addresses
    /// are rejected because this identity cannot carry their zone ID.
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
            Self::SourceIp(IpAddr::V4(address)) => is_unicast_v4(*address),
            Self::SourceIp(IpAddr::V6(address)) => {
                !address.is_unspecified() && !address.is_multicast() && !is_scoped_unicast(*address)
            }
        }
    }
}

pub(crate) const fn is_scoped_unicast(address: std::net::Ipv6Addr) -> bool {
    address.is_unicast_link_local() || address.segments()[0] & 0xffc0 == 0xfec0
}

pub(crate) const fn is_unicast_v4(address: std::net::Ipv4Addr) -> bool {
    let first = address.octets()[0];
    first != 0 && first < 224
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

    #[test]
    fn rejects_scoped_ipv6_unicast_but_keeps_unique_local_identity() {
        for address in [
            "fe80::1".parse::<Ipv6Addr>().expect("first link-local"),
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
                .parse::<Ipv6Addr>()
                .expect("last link-local"),
            "fec0::1".parse::<Ipv6Addr>().expect("first site-local"),
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"
                .parse::<Ipv6Addr>()
                .expect("last site-local"),
        ] {
            assert!(!PeerIdentity::SourceIp(IpAddr::V6(address)).is_attachable());
        }
        assert!(
            PeerIdentity::SourceIp(IpAddr::V6(
                "fdff:ffff::1".parse().expect("unique-local address")
            ))
            .is_attachable()
        );
    }

    #[test]
    fn rejects_non_unicast_ipv4_source_classes() {
        for address in [
            "0.0.0.0",
            "0.0.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.254",
            "255.255.255.255",
        ] {
            assert!(
                !PeerIdentity::SourceIp(IpAddr::V4(address.parse().expect("test IPv4")))
                    .is_attachable(),
                "accepted source identity {address}"
            );
        }
        for address in ["10.0.0.1", "127.0.0.1", "169.254.1.1", "223.255.255.254"] {
            assert!(
                PeerIdentity::SourceIp(IpAddr::V4(address.parse().expect("test IPv4")))
                    .is_attachable(),
                "rejected source identity {address}"
            );
        }
    }
}
