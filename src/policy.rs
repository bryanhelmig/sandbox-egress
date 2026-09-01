use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use ipnet::IpNet;

use crate::PolicyError;

/// How a visible `ClientHello` is related to CONNECT authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TlsAuthority {
    /// Do not inspect tunnel bytes after CONNECT.
    #[default]
    Disabled,
    /// Require a valid `ClientHello` whose visible SNI equals CONNECT authority.
    RequireVisibleSni {
        /// How an encrypted `ClientHello` extension is handled.
        ech: EchPolicy,
    },
}

/// Handling for TLS Encrypted `ClientHello` (ECH).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EchPolicy {
    /// Reject ECH because its inner authority is not visible to the proxy.
    #[default]
    Reject,
    /// Enforce only the visible outer SNI and allow an unknowable inner name.
    AllowOuterSni,
}

/// A canonical hostname pattern accepted by a [`Policy`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum HostPattern {
    /// Match one canonical hostname exactly.
    Exact(String),
    /// Match subdomains, but not the suffix apex itself.
    Subdomains(String),
}

impl HostPattern {
    /// Parse an ASCII hostname or a single left-most wildcard such as
    /// `*.example.com`.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::InvalidHostPattern`] for Unicode, malformed DNS
    /// labels, IP literals, or wildcards in any other position.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, PolicyError> {
        let value = value.as_ref();
        let (wildcard, hostname) = match value.strip_prefix("*.") {
            Some(hostname) => (true, hostname),
            None => (false, value),
        };
        let hostname = canonical_hostname(hostname)
            .ok_or_else(|| PolicyError::InvalidHostPattern(value.to_owned()))?;
        Ok(if wildcard {
            Self::Subdomains(hostname)
        } else {
            Self::Exact(hostname)
        })
    }

    pub(crate) fn matches(&self, hostname: &str) -> bool {
        match self {
            Self::Exact(expected) => hostname == expected,
            Self::Subdomains(suffix) => {
                hostname.len() > suffix.len()
                    && hostname.ends_with(suffix)
                    && hostname.as_bytes()[hostname.len() - suffix.len() - 1] == b'.'
            }
        }
    }
}

/// Immutable egress rules for one lease.
#[derive(Clone, Debug)]
pub struct Policy {
    pub(crate) hosts: Vec<HostPattern>,
    pub(crate) ports: BTreeSet<u16>,
    pub(crate) allowed_networks: Vec<IpNet>,
    pub(crate) max_connections: usize,
    pub(crate) dns_timeout: Duration,
    pub(crate) handshake_timeout: Duration,
    pub(crate) max_upload_bytes: Option<u64>,
    pub(crate) max_download_bytes: Option<u64>,
    pub(crate) tls_authority: TlsAuthority,
}

impl Policy {
    /// Begin a deny-by-default policy.
    pub fn builder() -> PolicyBuilder {
        PolicyBuilder::default()
    }

    pub(crate) fn allows_hostname(&self, hostname: &str) -> bool {
        self.hosts.iter().any(|pattern| pattern.matches(hostname))
    }

    pub(crate) fn allows_port(&self, port: u16) -> bool {
        self.ports.contains(&port)
    }

    pub(crate) fn allows_ip(&self, address: IpAddr) -> bool {
        if self
            .allowed_networks
            .iter()
            .any(|network| network.contains(&address))
        {
            return true;
        }
        !is_forbidden_destination(address)
    }

    pub(crate) fn allows_ip_literal(&self, address: IpAddr) -> bool {
        self.allowed_networks
            .iter()
            .any(|network| network.contains(&address))
    }
}

/// Builder for an immutable [`Policy`].
#[derive(Clone, Debug)]
#[must_use]
pub struct PolicyBuilder {
    hosts: Vec<HostPattern>,
    ports: BTreeSet<u16>,
    allowed_networks: Vec<IpNet>,
    max_connections: usize,
    dns_timeout: Duration,
    handshake_timeout: Duration,
    max_upload_bytes: Option<u64>,
    max_download_bytes: Option<u64>,
    tls_authority: TlsAuthority,
}

impl PolicyBuilder {
    /// Add an exact hostname or `*.example.com` subdomain pattern.
    ///
    /// # Errors
    ///
    /// Returns an error when the pattern is not a canonical ASCII DNS pattern.
    pub fn allow_host(mut self, pattern: impl AsRef<str>) -> Result<Self, PolicyError> {
        self.hosts.push(HostPattern::parse(pattern)?);
        Ok(self)
    }

    /// Allow CONNECT to a destination port.
    pub fn allow_port(mut self, port: u16) -> Self {
        self.ports.insert(port);
        self
    }

    /// Explicitly allow a destination network, overriding the default
    /// forbidden-address floor for that network.
    pub fn allow_network(mut self, network: IpNet) -> Self {
        self.allowed_networks.push(network);
        self
    }

    /// Set the concurrent connection ceiling for this lease.
    ///
    /// # Errors
    ///
    /// Returns an error when `max` is zero or too large for the runtime's
    /// admission semaphore.
    pub fn max_connections(mut self, max: usize) -> Result<Self, PolicyError> {
        if max == 0 {
            return Err(PolicyError::ZeroConnectionLimit);
        }
        if max > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(PolicyError::ConnectionLimitTooLarge);
        }
        self.max_connections = max;
        Ok(self)
    }

    /// Set the DNS deadline.
    pub fn dns_timeout(mut self, timeout: Duration) -> Self {
        self.dns_timeout = timeout;
        self
    }

    /// Set the absolute header + DNS + dial + optional `ClientHello` deadline.
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set the maximum bytes uploaded over one tunnel.
    pub fn max_upload_bytes(mut self, bytes: u64) -> Self {
        self.max_upload_bytes = Some(bytes);
        self
    }

    /// Set the maximum bytes downloaded over one tunnel.
    pub fn max_download_bytes(mut self, bytes: u64) -> Self {
        self.max_download_bytes = Some(bytes);
        self
    }

    /// Require the tunnel to begin with a valid `ClientHello` whose visible SNI
    /// equals its hostname CONNECT authority, and reject ECH.
    pub fn require_tls_sni(mut self) -> Self {
        self.tls_authority = TlsAuthority::RequireVisibleSni {
            ech: EchPolicy::Reject,
        };
        self
    }

    /// Configure TLS authority inspection explicitly.
    pub fn tls_authority(mut self, authority: TlsAuthority) -> Self {
        self.tls_authority = authority;
        self
    }

    /// Validate and freeze the policy.
    ///
    /// # Errors
    ///
    /// Returns an error for zero deadlines, a DNS deadline longer than the
    /// complete handshake deadline, or a timeout too large for a runtime
    /// deadline.
    pub fn build(self) -> Result<Policy, PolicyError> {
        if self.dns_timeout.is_zero() || self.handshake_timeout.is_zero() {
            return Err(PolicyError::ZeroTimeout);
        }
        if self.dns_timeout > self.handshake_timeout {
            return Err(PolicyError::DnsTimeoutExceedsHandshake);
        }
        if Instant::now().checked_add(self.handshake_timeout).is_none() {
            return Err(PolicyError::TimeoutTooLarge);
        }
        Ok(Policy {
            hosts: self.hosts,
            ports: self.ports,
            allowed_networks: self.allowed_networks,
            max_connections: self.max_connections,
            dns_timeout: self.dns_timeout,
            handshake_timeout: self.handshake_timeout,
            max_upload_bytes: self.max_upload_bytes,
            max_download_bytes: self.max_download_bytes,
            tls_authority: self.tls_authority,
        })
    }
}

impl Default for PolicyBuilder {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            ports: BTreeSet::from([443]),
            allowed_networks: Vec::new(),
            max_connections: 64,
            dns_timeout: Duration::from_secs(3),
            handshake_timeout: Duration::from_secs(10),
            max_upload_bytes: None,
            max_download_bytes: None,
            tls_authority: TlsAuthority::Disabled,
        }
    }
}

pub(crate) fn canonical_hostname(value: &str) -> Option<String> {
    let value = value.strip_suffix('.').unwrap_or(value);
    if value.is_empty() || value.len() > 253 || !value.is_ascii() || value.parse::<IpAddr>().is_ok()
    {
        return None;
    }
    let canonical = value.to_ascii_lowercase();
    for label in canonical.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
    }
    Some(canonical)
}

pub(crate) fn is_forbidden_destination(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => forbidden_v4(address),
        IpAddr::V6(address) => {
            if let Some(embedded) = address.to_ipv4() {
                return forbidden_v4(embedded);
            }
            if is_well_known_nat64(address) {
                let octets = address.octets();
                return forbidden_v4(Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                ));
            }
            forbidden_v6(address)
        }
    }
}

fn is_well_known_nat64(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0]
}

// Derived from the IANA special-purpose registries, reviewed 2026-08-31.
const FORBIDDEN_V4_PREFIXES: &[(u32, u32)] = &[
    (0x0000_0000, 8),  // 0.0.0.0/8, this network
    (0x0a00_0000, 8),  // 10.0.0.0/8, private
    (0x6440_0000, 10), // 100.64.0.0/10, shared
    (0x7f00_0000, 8),  // 127.0.0.0/8, loopback
    (0xa9fe_0000, 16), // 169.254.0.0/16, link-local
    (0xac10_0000, 12), // 172.16.0.0/12, private
    (0xc000_0000, 24), // 192.0.0.0/24, protocol assignments
    (0xc000_0200, 24), // 192.0.2.0/24, documentation
    (0xc058_6300, 24), // 192.88.99.0/24, deprecated 6to4 relay
    (0xc0a8_0000, 16), // 192.168.0.0/16, private
    (0xc612_0000, 15), // 198.18.0.0/15, benchmarking
    (0xc633_6400, 24), // 198.51.100.0/24, documentation
    (0xcb00_7100, 24), // 203.0.113.0/24, documentation
    (0xe000_0000, 3),  // multicast and reserved
];

const FORBIDDEN_V6_PREFIXES: &[(u128, u32)] = &[
    (0x2001_0000_0000_0000_0000_0000_0000_0000, 23), // IETF assignments umbrella
    (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32), // documentation
    (0x2002_0000_0000_0000_0000_0000_0000_0000, 16), // 6to4
    (0x3fff_0000_0000_0000_0000_0000_0000_0000, 20), // documentation
];

fn forbidden_v4(address: Ipv4Addr) -> bool {
    FORBIDDEN_V4_PREFIXES
        .iter()
        .any(|&(network, length)| prefix_matches(u32::from(address), network, length, 32))
}

fn forbidden_v6(address: Ipv6Addr) -> bool {
    !prefix_matches(
        u128::from(address),
        0x2000_0000_0000_0000_0000_0000_0000_0000,
        3,
        128,
    ) || FORBIDDEN_V6_PREFIXES
        .iter()
        .any(|&(network, length)| prefix_matches(u128::from(address), network, length, 128))
}

fn prefix_matches<T>(address: T, network: T, length: u32, width: u32) -> bool
where
    T: Copy + PartialEq + std::ops::Shr<u32, Output = T>,
{
    let shift = width - length;
    address >> shift == network >> shift
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_excludes_the_apex() {
        let pattern = HostPattern::parse("*.Example.COM.").expect("valid pattern");
        assert!(pattern.matches("api.example.com"));
        assert!(!pattern.matches("example.com"));
        assert!(!pattern.matches("notexample.com"));
    }

    #[test]
    fn rejects_unrepresentable_handshake_deadline() {
        assert_eq!(
            Policy::builder()
                .dns_timeout(Duration::MAX)
                .handshake_timeout(Duration::MAX)
                .build()
                .unwrap_err(),
            PolicyError::TimeoutTooLarge
        );
    }

    #[test]
    fn rejects_a_connection_limit_above_the_runtime_bound() {
        Policy::builder()
            .max_connections(tokio::sync::Semaphore::MAX_PERMITS)
            .expect("runtime maximum is valid")
            .build()
            .expect("boundary policy");
        assert_eq!(
            Policy::builder().max_connections(usize::MAX).unwrap_err(),
            PolicyError::ConnectionLimitTooLarge
        );
    }

    #[test]
    fn private_and_metadata_destinations_are_forbidden() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "100:0:0:1::1",
            "2001::1",
            "2001:2::1",
            "2001:5::1",
            "2001:10::1",
            "2001:20::1",
            "2001:30::1",
            "2002::1",
            "3fff::1",
            "5f00::1",
            "4000::1",
            "8000::1",
            "fe00::1",
        ] {
            let address = address.parse().expect("test IP");
            assert!(is_forbidden_destination(address), "{address}");
        }
        assert!(!is_forbidden_destination(
            "93.184.216.34".parse().expect("test IP")
        ));
        assert!(!is_forbidden_destination(
            "::93.184.216.34"
                .parse()
                .expect("compatible public test IP")
        ));
        assert!(!is_forbidden_destination(
            "64:ff9b::5db8:d822".parse().expect("NAT64 public test IP")
        ));
        assert!(!is_forbidden_destination(
            "2606:4700:4700::1111"
                .parse()
                .expect("native public IPv6 test IP")
        ));
    }

    #[test]
    fn every_forbidden_prefix_includes_its_first_and_last_address() {
        for &(network, length) in FORBIDDEN_V4_PREFIXES {
            let host_mask = u32::MAX >> length;
            assert!(
                forbidden_v4(Ipv4Addr::from(network)),
                "{network:#010x}/{length}"
            );
            assert!(
                forbidden_v4(Ipv4Addr::from(network | host_mask)),
                "{network:#010x}/{length}"
            );
        }
        for &(network, length) in FORBIDDEN_V6_PREFIXES {
            let host_mask = u128::MAX >> length;
            assert!(
                forbidden_v6(Ipv6Addr::from(network)),
                "{network:#034x}/{length}"
            );
            assert!(
                forbidden_v6(Ipv6Addr::from(network | host_mask)),
                "{network:#034x}/{length}"
            );
        }
    }

    #[test]
    fn explicit_network_grant_overrides_the_special_purpose_floor() {
        let address: IpAddr = "2001:5::1".parse().expect("special-purpose test IP");
        let policy = Policy::builder()
            .allow_network("2001:5::1/128".parse().expect("explicit test grant"))
            .build()
            .expect("valid policy");

        assert!(policy.allows_ip(address));
        assert!(policy.allows_ip_literal(address));
    }
}
