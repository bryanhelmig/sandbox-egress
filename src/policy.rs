use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use ipnet::{IpNet, Ipv6Net};

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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum HostPattern {
    Exact(String),
    Subdomains(String),
}

impl HostPattern {
    fn parse(value: impl AsRef<str>) -> Result<Self, PolicyError> {
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
    pub(crate) denied_hosts: Vec<HostPattern>,
    pub(crate) ports: BTreeSet<u16>,
    pub(crate) allowed_networks: Vec<IpNet>,
    pub(crate) denied_networks: Vec<IpNet>,
    pub(crate) max_connections: usize,
    pub(crate) dns_timeout: Duration,
    pub(crate) handshake_timeout: Duration,
    pub(crate) idle_timeout: Option<Duration>,
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
        !self
            .denied_hosts
            .iter()
            .any(|pattern| pattern.matches(hostname))
            && self.hosts.iter().any(|pattern| pattern.matches(hostname))
    }

    pub(crate) fn allows_port(&self, port: u16) -> bool {
        self.ports.contains(&port)
    }

    pub(crate) fn allows_ip(&self, address: IpAddr, nat64_prefixes: &[Ipv6Net]) -> bool {
        if address_matches_networks(&self.denied_networks, address, nat64_prefixes) {
            return false;
        }
        if self
            .allowed_networks
            .iter()
            .any(|network| network.contains(&address))
        {
            return true;
        }
        !is_forbidden_destination(address, nat64_prefixes)
    }

    pub(crate) fn allows_ip_literal(&self, address: IpAddr, nat64_prefixes: &[Ipv6Net]) -> bool {
        !address_matches_networks(&self.denied_networks, address, nat64_prefixes)
            && self
                .allowed_networks
                .iter()
                .any(|network| network.contains(&address))
    }
}

fn address_matches_networks(
    networks: &[IpNet],
    address: IpAddr,
    nat64_prefixes: &[Ipv6Net],
) -> bool {
    networks
        .iter()
        .any(|network| address_matches_network(network, address, nat64_prefixes))
}

fn address_matches_network(network: &IpNet, address: IpAddr, nat64_prefixes: &[Ipv6Net]) -> bool {
    network.contains(&address)
        || match address {
            IpAddr::V4(_) => false,
            IpAddr::V6(address) => translated_ipv4_matches(address, nat64_prefixes, |embedded| {
                network.contains(&IpAddr::V4(embedded))
            })
            .unwrap_or(false),
        }
}

/// Builder for an immutable [`Policy`].
#[derive(Clone, Debug)]
#[must_use]
pub struct PolicyBuilder {
    policy: Policy,
}

impl PolicyBuilder {
    /// Add an exact hostname or `*.example.com` pattern matching subdomains at
    /// any depth, but not `example.com` itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the pattern is not a canonical ASCII DNS pattern.
    pub fn allow_host(mut self, pattern: impl AsRef<str>) -> Result<Self, PolicyError> {
        self.policy.hosts.push(HostPattern::parse(pattern)?);
        Ok(self)
    }

    /// Deny an exact hostname or `*.example.com` pattern even if another rule
    /// grants it.
    ///
    /// # Errors
    ///
    /// Returns an error when the pattern is not a canonical ASCII DNS pattern.
    pub fn deny_host(mut self, pattern: impl AsRef<str>) -> Result<Self, PolicyError> {
        self.policy.denied_hosts.push(HostPattern::parse(pattern)?);
        Ok(self)
    }

    /// Allow CONNECT to a destination port. No port is allowed until added.
    pub fn allow_port(mut self, port: u16) -> Self {
        self.policy.ports.insert(port);
        self
    }

    /// Explicitly allow a destination network, overriding the default
    /// forbidden-address floor for that network unless it is also denied.
    pub fn allow_network(mut self, network: IpNet) -> Self {
        self.policy.allowed_networks.push(network);
        self
    }

    /// Explicitly deny a destination network.
    ///
    /// Denial takes precedence over [`PolicyBuilder::allow_network`] and the
    /// default public-address behavior. An IPv4 denial also covers mapped,
    /// compatible, and configured NAT64 forms of that effective destination.
    pub fn deny_network(mut self, network: IpNet) -> Self {
        self.policy.denied_networks.push(network);
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
        self.policy.max_connections = max;
        Ok(self)
    }

    /// Set the DNS deadline.
    pub fn dns_timeout(mut self, timeout: Duration) -> Self {
        self.policy.dns_timeout = timeout;
        self
    }

    /// Set the absolute header + DNS + dial + optional `ClientHello` deadline.
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.policy.handshake_timeout = timeout;
        self
    }

    /// End an established tunnel after this duration passes without bytes in
    /// either direction. Idle expiry is disabled unless configured.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.policy.idle_timeout = Some(timeout);
        self
    }

    /// Set the maximum bytes uploaded over one tunnel.
    ///
    /// After CONNECT succeeds, the proxy forwards exactly the permitted
    /// prefix. A nonempty read after the limit is reached is accounted,
    /// rejected, and closes the tunnel.
    pub fn max_upload_bytes(mut self, bytes: u64) -> Self {
        self.policy.max_upload_bytes = Some(bytes);
        self
    }

    /// Set the maximum bytes downloaded over one tunnel.
    ///
    /// After CONNECT succeeds, the proxy forwards exactly the permitted
    /// prefix. A nonempty read after the limit is reached is accounted,
    /// rejected, and closes the tunnel.
    pub fn max_download_bytes(mut self, bytes: u64) -> Self {
        self.policy.max_download_bytes = Some(bytes);
        self
    }

    /// Require the tunnel to begin with a valid `ClientHello` whose visible SNI
    /// equals its hostname CONNECT authority, and reject ECH.
    pub fn require_tls_sni(mut self) -> Self {
        self.policy.tls_authority = TlsAuthority::RequireVisibleSni {
            ech: EchPolicy::Reject,
        };
        self
    }

    /// Configure TLS authority inspection explicitly.
    pub fn tls_authority(mut self, authority: TlsAuthority) -> Self {
        self.policy.tls_authority = authority;
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
        let policy = self.policy;
        if policy.dns_timeout.is_zero()
            || policy.handshake_timeout.is_zero()
            || policy.idle_timeout.is_some_and(|timeout| timeout.is_zero())
        {
            return Err(PolicyError::ZeroTimeout);
        }
        if policy.dns_timeout > policy.handshake_timeout {
            return Err(PolicyError::DnsTimeoutExceedsHandshake);
        }
        let now = Instant::now();
        if now.checked_add(policy.handshake_timeout).is_none() {
            return Err(PolicyError::TimeoutTooLarge);
        }
        if policy
            .idle_timeout
            .is_some_and(|timeout| now.checked_add(timeout).is_none())
        {
            return Err(PolicyError::TimeoutTooLarge);
        }
        Ok(policy)
    }
}

impl Default for PolicyBuilder {
    fn default() -> Self {
        Self {
            policy: Policy {
                hosts: Vec::new(),
                denied_hosts: Vec::new(),
                ports: BTreeSet::new(),
                allowed_networks: Vec::new(),
                denied_networks: Vec::new(),
                max_connections: 64,
                dns_timeout: Duration::from_secs(3),
                handshake_timeout: Duration::from_secs(10),
                idle_timeout: None,
                max_upload_bytes: None,
                max_download_bytes: None,
                tls_authority: TlsAuthority::Disabled,
            },
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

pub(crate) fn is_forbidden_destination(address: IpAddr, nat64_prefixes: &[Ipv6Net]) -> bool {
    match address {
        IpAddr::V4(address) => forbidden_v4(address),
        IpAddr::V6(address) => translated_ipv4_matches(address, nat64_prefixes, forbidden_v4)
            .unwrap_or_else(|| forbidden_v6(address)),
    }
}

fn translated_ipv4_matches(
    address: Ipv6Addr,
    nat64_prefixes: &[Ipv6Net],
    mut predicate: impl FnMut(Ipv4Addr) -> bool,
) -> Option<bool> {
    if let Some(embedded) = address.to_ipv4() {
        return Some(predicate(embedded));
    }
    if is_well_known_nat64(address) {
        return Some(predicate(extract_rfc6052_ipv4(address, 96)));
    }
    let mut translated = false;
    for prefix in nat64_prefixes {
        if prefix.contains(&address) {
            translated = true;
            if predicate(extract_rfc6052_ipv4(address, prefix.prefix_len())) {
                return Some(true);
            }
        }
    }
    translated.then_some(false)
}

fn is_well_known_nat64(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0]
}

fn extract_rfc6052_ipv4(address: Ipv6Addr, prefix_len: u8) -> Ipv4Addr {
    let bytes = address.octets();
    let octets = match prefix_len {
        32 => [bytes[4], bytes[5], bytes[6], bytes[7]],
        40 => [bytes[5], bytes[6], bytes[7], bytes[9]],
        48 => [bytes[6], bytes[7], bytes[9], bytes[10]],
        56 => [bytes[7], bytes[9], bytes[10], bytes[11]],
        64 => [bytes[9], bytes[10], bytes[11], bytes[12]],
        96 => [bytes[12], bytes[13], bytes[14], bytes[15]],
        _ => unreachable!("ProxyConfig validates RFC 6052 prefix lengths"),
    };
    Ipv4Addr::from(octets)
}

// Derived from the IANA special-purpose registries plus reviewed provider
// control-plane endpoints, reviewed 2026-09-01.
const FORBIDDEN_V4_PREFIXES: &[(u32, u32)] = &[
    (0x0000_0000, 8),  // 0.0.0.0/8, this network
    (0x0a00_0000, 8),  // 10.0.0.0/8, private
    (0x6440_0000, 10), // 100.64.0.0/10, shared
    (0x7f00_0000, 8),  // 127.0.0.0/8, loopback
    (0xa83f_8110, 32), // 168.63.129.16/32, Azure WireServer
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
    fn wildcard_matches_every_subdomain_depth_but_excludes_the_apex() {
        let pattern = HostPattern::parse("*.Example.COM.").expect("valid pattern");
        assert!(pattern.matches("api.example.com"));
        assert!(pattern.matches("deep.api.example.com"));
        assert!(!pattern.matches("example.com"));
        assert!(!pattern.matches("notexample.com"));
    }

    #[test]
    fn hostname_denial_overrides_exact_and_wildcard_grants() {
        let policy = Policy::builder()
            .allow_host("*.example.com")
            .expect("valid wildcard grant")
            .allow_host("admin.example.com")
            .expect("valid exact grant")
            .deny_host("admin.example.com")
            .expect("valid exact denial")
            .deny_host("*.internal.example.com")
            .expect("valid wildcard denial")
            .build()
            .expect("valid policy");

        assert!(policy.allows_hostname("api.example.com"));
        assert!(policy.allows_hostname("deep.api.example.com"));
        assert!(policy.allows_hostname("internal.example.com"));
        assert!(!policy.allows_hostname("admin.example.com"));
        assert!(!policy.allows_hostname("deep.internal.example.com"));
        assert!(!policy.allows_hostname("example.com"));
    }

    #[test]
    fn ascii_hostname_canonicalization_has_explicit_dns_boundaries() {
        assert_eq!(
            canonical_hostname("API.Example.COM."),
            Some("api.example.com".to_owned())
        );
        assert_eq!(
            canonical_hostname("xn--bcher-kva.example"),
            Some("xn--bcher-kva.example".to_owned())
        );

        let longest_label = "a".repeat(63);
        assert!(canonical_hostname(&format!("{longest_label}.example")).is_some());
        assert!(canonical_hostname(&format!("{}.example", "a".repeat(64))).is_none());

        let longest_name = format!(
            "{}.{}.{}.{}.",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(longest_name.len(), 254);
        assert!(canonical_hostname(&longest_name).is_some());
        assert!(canonical_hostname(&format!("{}e", longest_name.trim_end_matches('.'))).is_none());

        for invalid in [
            "bücher.example",
            "exаmple.com",
            "under_score.example",
            "example.com..",
            "-leading.example",
            "trailing-.example",
            "127.0.0.1",
        ] {
            assert!(
                canonical_hostname(invalid).is_none(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn ports_are_empty_until_explicitly_allowed() {
        let empty = Policy::builder().build().expect("valid empty policy");
        assert!(!empty.allows_port(443));

        let http_only = Policy::builder()
            .allow_port(80)
            .build()
            .expect("valid HTTP-only policy");
        assert!(http_only.allows_port(80));
        assert!(!http_only.allows_port(443));
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
    fn rejects_invalid_idle_deadlines() {
        assert_eq!(
            Policy::builder()
                .idle_timeout(Duration::ZERO)
                .build()
                .unwrap_err(),
            PolicyError::ZeroTimeout
        );
        assert_eq!(
            Policy::builder()
                .idle_timeout(Duration::MAX)
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
            "168.63.129.16",
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
            assert!(is_forbidden_destination(address, &[]), "{address}");
        }
        assert!(!is_forbidden_destination(
            "93.184.216.34".parse().expect("test IP"),
            &[],
        ));
        assert!(!is_forbidden_destination(
            "::93.184.216.34"
                .parse()
                .expect("compatible public test IP"),
            &[],
        ));
        assert!(!is_forbidden_destination(
            "64:ff9b::5db8:d822".parse().expect("NAT64 public test IP"),
            &[],
        ));
        assert!(!is_forbidden_destination(
            "2606:4700:4700::1111"
                .parse()
                .expect("native public IPv6 test IP"),
            &[],
        ));
    }

    #[test]
    fn extracts_every_rfc6052_network_specific_layout() {
        for (address, prefix_len) in [
            ("2001:db8:c000:221::", 32),
            ("2001:db8:1c0:2:21::", 40),
            ("2001:db8:122:c000:2:2100::", 48),
            ("2001:db8:122:3c0:0:221::", 56),
            ("2001:db8:122:344:c0:2:2100::", 64),
            ("2001:db8:122:344::c000:221", 96),
        ] {
            assert_eq!(
                extract_rfc6052_ipv4(address.parse().expect("RFC 6052 example"), prefix_len),
                Ipv4Addr::new(192, 0, 2, 33),
                "{address}/{prefix_len}"
            );
        }
    }

    #[test]
    fn configured_nat64_prefix_checks_the_effective_ipv4_destination() {
        let prefix: Ipv6Net = "2600:1f18:abcd:1234::/96"
            .parse()
            .expect("network-specific NAT64 prefix");
        let metadata = "2600:1f18:abcd:1234::a9fe:a9fe"
            .parse()
            .expect("translated metadata address");
        let public = "2600:1f18:abcd:1234::5db8:d822"
            .parse()
            .expect("translated public address");

        assert!(!is_forbidden_destination(metadata, &[]));
        assert!(is_forbidden_destination(metadata, &[prefix]));
        assert!(!is_forbidden_destination(public, &[prefix]));

        let override_policy = Policy::builder()
            .allow_network(
                "2600:1f18:abcd:1234::a9fe:a9fe/128"
                    .parse()
                    .expect("explicit translated metadata grant"),
            )
            .build()
            .expect("valid override policy");
        assert!(override_policy.allows_ip(metadata, &[prefix]));
    }

    #[test]
    fn every_forbidden_prefix_includes_its_first_and_last_address() {
        for &(network, length) in FORBIDDEN_V4_PREFIXES {
            let host_mask = u32::MAX.checked_shr(length).unwrap_or(0);
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
            let host_mask = u128::MAX.checked_shr(length).unwrap_or(0);
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

        assert!(policy.allows_ip(address, &[]));
        assert!(policy.allows_ip_literal(address, &[]));
    }

    #[test]
    fn explicit_network_denial_overrides_every_grant_path() {
        let nat64_prefix = "2001:db8:64::/96"
            .parse::<Ipv6Net>()
            .expect("valid test NAT64 prefix");
        let policy = Policy::builder()
            .allow_network("0.0.0.0/0".parse().expect("valid catch-all grant"))
            .allow_network("::/0".parse().expect("valid IPv6 catch-all grant"))
            .deny_network(
                "93.184.216.0/24"
                    .parse()
                    .expect("valid public denial network"),
            )
            .build()
            .expect("valid policy");
        let allowed = "1.1.1.1".parse().expect("public allowed address");

        for denied in [
            "93.184.216.34",
            "::ffff:93.184.216.34",
            "::93.184.216.34",
            "64:ff9b::5db8:d822",
            "2001:db8:64::5db8:d822",
        ] {
            let denied = denied.parse().expect("public denied address form");
            assert!(
                !policy.allows_ip(denied, std::slice::from_ref(&nat64_prefix)),
                "allowed denied address form {denied}"
            );
            assert!(
                !policy.allows_ip_literal(denied, std::slice::from_ref(&nat64_prefix)),
                "allowed denied literal form {denied}"
            );
        }
        assert!(policy.allows_ip(allowed, &[]));
        assert!(policy.allows_ip_literal(allowed, &[]));
    }
}
