use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use ipnet::IpNet;

use crate::PolicyError;

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
    /// Returns an error when `max` is zero.
    pub fn max_connections(mut self, max: usize) -> Result<Self, PolicyError> {
        if max == 0 {
            return Err(PolicyError::ZeroConnectionLimit);
        }
        self.max_connections = max;
        Ok(self)
    }

    /// Set the DNS deadline.
    pub fn dns_timeout(mut self, timeout: Duration) -> Self {
        self.dns_timeout = timeout;
        self
    }

    /// Set the absolute header + DNS + dial deadline.
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

    /// Validate and freeze the policy.
    ///
    /// # Errors
    ///
    /// Returns an error for zero deadlines or a DNS deadline longer than the
    /// complete handshake deadline.
    pub fn build(self) -> Result<Policy, PolicyError> {
        if self.dns_timeout.is_zero() || self.handshake_timeout.is_zero() {
            return Err(PolicyError::ZeroTimeout);
        }
        if self.dns_timeout > self.handshake_timeout {
            return Err(PolicyError::DnsTimeoutExceedsHandshake);
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
            if let Some(mapped) = address.to_ipv4_mapped() {
                return forbidden_v4(mapped);
            }
            forbidden_v6(address)
        }
    }
}

fn forbidden_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
        || (a == 255 && b == 255 && c == 255 && d == 255)
}

fn forbidden_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
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
    fn private_and_metadata_destinations_are_forbidden() {
        for address in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "::1", "fc00::1"] {
            let address = address.parse().expect("test IP");
            assert!(is_forbidden_destination(address), "{address}");
        }
        assert!(!is_forbidden_destination(
            "93.184.216.34".parse().expect("test IP")
        ));
    }
}
