use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use ipnet::Ipv6Net;

use crate::DiagnosticEvent;
use crate::diagnostic::DiagnosticConfig;
use crate::identity::{is_scoped_unicast, is_unicast_v4};
use crate::rate::RateLimit;

pub(crate) const MAX_DNS_CACHE_ENTRIES: u64 = 64;
pub(crate) const MAX_DNS_CACHE_TTL: Duration = Duration::from_secs(86_400);
pub(crate) const MAX_DNS_SERVERS: usize = 8;

/// Process-wide proxy configuration.
#[derive(Clone, Debug)]
#[must_use]
pub struct ProxyConfig {
    pub(crate) bind_address: SocketAddr,
    pub(crate) max_connections: usize,
    pub(crate) connection_attempt_rate: Option<RateLimit>,
    pub(crate) max_concurrent_dns: usize,
    pub(crate) max_concurrent_dials: usize,
    pub(crate) dns_cache_entries: u64,
    pub(crate) dns_cache_max_ttl: Duration,
    pub(crate) dns_servers: Vec<SocketAddr>,
    pub(crate) max_resolved_addresses: usize,
    pub(crate) max_header_bytes: usize,
    pub(crate) max_client_hello_bytes: usize,
    pub(crate) nat64_prefixes: Vec<Ipv6Net>,
    pub(crate) header_timeout: Duration,
    pub(crate) identity_reuse_quiet_period: Duration,
    pub(crate) diagnostics: Option<DiagnosticConfig>,
    pub(crate) upstream_proxy: Option<SocketAddr>,
}

impl ProxyConfig {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !is_bindable_listener(self.bind_address) {
            return Err("listener must be a unicast or wildcard address with any required zone");
        }
        if self.header_timeout.is_zero() {
            return Err("header timeout must be nonzero");
        }
        if self
            .connection_attempt_rate
            .is_some_and(|limit| !limit.is_valid())
        {
            return Err("connection attempt rate and burst must be nonzero");
        }
        let now = Instant::now();
        if now.checked_add(self.header_timeout).is_none() {
            return Err("header timeout is too large");
        }
        if now.checked_add(self.identity_reuse_quiet_period).is_none() {
            return Err("identity reuse quiet period is too large");
        }
        if self
            .nat64_prefixes
            .iter()
            .any(|prefix| !matches!(prefix.prefix_len(), 32 | 40 | 48 | 56 | 64 | 96))
        {
            return Err("NAT64 prefix length must be 32, 40, 48, 56, 64, or 96");
        }
        if self.dns_servers.len() > MAX_DNS_SERVERS {
            return Err("too many explicit DNS servers");
        }
        if self.dns_servers.iter().any(|server| server.port() == 0) {
            return Err("explicit DNS server port must be nonzero");
        }
        if self
            .dns_servers
            .iter()
            .any(|server| !is_concrete_unicast(*server))
        {
            return Err("explicit DNS server must be a concrete unicast address");
        }
        if self.upstream_proxy.is_some_and(|proxy| proxy.port() == 0) {
            return Err("upstream proxy port must be nonzero");
        }
        if self
            .upstream_proxy
            .is_some_and(|proxy| !is_concrete_unicast(proxy))
        {
            return Err("upstream proxy must be a concrete unicast address");
        }
        if self.dns_servers.iter().any(|server| match server {
            SocketAddr::V4(_) => false,
            SocketAddr::V6(server) => server.scope_id() != 0 || is_scoped_unicast(*server.ip()),
        }) {
            return Err("scoped IPv6 DNS servers are not supported");
        }
        if self.upstream_proxy.is_some_and(|proxy| match proxy {
            SocketAddr::V4(_) => false,
            SocketAddr::V6(proxy) => is_scoped_unicast(*proxy.ip()) && proxy.scope_id() == 0,
        }) {
            return Err("scoped IPv6 upstream proxy requires a zone ID");
        }
        Ok(())
    }

    /// Set the listener address. Port zero asks the operating system to choose.
    /// A wildcard bind rejects every destination using its assigned listener
    /// port because the proxy cannot distinguish remote addresses from its
    /// other local interfaces. Bind a concrete guest-facing address when runs
    /// must reach unrelated destinations on that port. Startup rejects
    /// multicast, limited broadcast, and scoped IPv6 without a zone. A
    /// wildcard remains visible in [`Endpoint`](crate::Endpoint); the host must
    /// advertise an address reachable from each guest with the assigned port.
    pub fn with_bind_address(mut self, address: SocketAddr) -> Self {
        self.bind_address = address;
        self
    }

    /// Route approved destinations through an operator-controlled HTTP CONNECT
    /// proxy at a numeric socket address.
    ///
    /// Destination names are still resolved and checked locally. The upstream
    /// proxy receives the approved numeric address, so it cannot perform a
    /// second destination lookup. Authentication and TLS to the upstream proxy
    /// are not provided by this configuration. Startup requires a concrete
    /// unicast address; scoped IPv6 additionally requires a zone identifier.
    pub fn with_upstream_proxy(mut self, address: SocketAddr) -> Self {
        self.upstream_proxy = Some(address);
        self
    }

    /// Set the process-wide concurrent connection ceiling, clamped to the
    /// runtime semaphore's safe range.
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        self
    }

    /// Set a process-wide token bucket for attributed inbound TCP attempts.
    ///
    /// The bucket starts full, refills at `rate_per_second`, and holds at most
    /// `burst` attempts. The check happens after source-IP attribution but
    /// before header parsing, task creation, or concurrent admission. This
    /// bounds rapid terminal connection churn in addition to the concurrent
    /// connection ceiling. Zero disables neither value:
    /// [`Proxy::start`](crate::Proxy::start) rejects it.
    pub fn with_connection_attempt_rate(mut self, rate_per_second: u32, burst: u32) -> Self {
        self.connection_attempt_rate = Some(RateLimit::new(rate_per_second, burst));
        self
    }

    /// Set the process-wide ceiling for DNS lookups executing concurrently,
    /// clamped to the runtime semaphore's safe range.
    /// Connections waiting for a permit remain subject to their DNS and
    /// absolute handshake deadlines.
    pub fn with_max_concurrent_dns(mut self, max: usize) -> Self {
        self.max_concurrent_dns = max.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        self
    }

    /// Set the process-wide ceiling for outbound connection attempts executing
    /// concurrently, clamped to the runtime semaphore's safe range.
    /// Waiting for a permit remains subject to the absolute handshake deadline;
    /// a permit is released as soon as dialing finishes, before tunnelling.
    pub fn with_max_concurrent_dials(mut self, max: usize) -> Self {
        self.max_concurrent_dials = max.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
        self
    }

    /// Opt into a process-wide resolver cache bounded by response count and TTL.
    ///
    /// Caching is disabled by default because a DNS response can contain many
    /// records and the resolver bounds its cache by response count, not bytes.
    /// The values cannot exceed 64 responses and 24 hours. The TTL ceiling
    /// applies to both successful and negative answers. Set `entries` to zero
    /// to disable the cache.
    pub fn with_dns_cache(mut self, entries: u64, max_ttl: Duration) -> Self {
        self.dns_cache_entries = entries.min(MAX_DNS_CACHE_ENTRIES);
        self.dns_cache_max_ttl = max_ttl.min(MAX_DNS_CACHE_TTL);
        self
    }

    /// Add an explicit recursive DNS server for this proxy process.
    ///
    /// When at least one server is supplied, the resolver does not read the
    /// host's resolver configuration or hosts file. Each server is contacted
    /// over UDP with TCP available for responses that require fallback. The
    /// address and port are host-controlled process configuration, never a
    /// guest or lease policy selector.
    ///
    /// Duplicate socket addresses are ignored. [`Proxy::start`](crate::Proxy::start)
    /// rejects more than eight distinct servers, non-concrete addresses, and
    /// scoped IPv6 addresses.
    pub fn with_dns_server(mut self, server: SocketAddr) -> Self {
        if !self.dns_servers.contains(&server) {
            self.dns_servers.push(server);
        }
        self
    }

    /// Set the maximum IP addresses accepted from one DNS lookup.
    ///
    /// Values are clamped to `1..=1024`. An answer over the configured ceiling
    /// is rejected as a whole rather than partially dialed.
    pub fn with_max_resolved_addresses(mut self, max: usize) -> Self {
        self.max_resolved_addresses = max.clamp(1, 1_024);
        self
    }

    /// Set the maximum CONNECT header block size.
    pub fn with_max_header_bytes(mut self, bytes: usize) -> Self {
        self.max_header_bytes = bytes.clamp(1_024, 1024 * 1024);
        self
    }

    /// Set the maximum buffered TLS `ClientHello` size for inspected tunnels.
    pub fn with_max_client_hello_bytes(mut self, bytes: usize) -> Self {
        self.max_client_hello_bytes = bytes.clamp(1_024, 1024 * 1024);
        self
    }

    /// Register a network-specific NAT64 prefix whose embedded IPv4 address
    /// must receive the ordinary forbidden-destination checks.
    ///
    /// RFC 6052 permits prefix lengths of `/32`, `/40`, `/48`, `/56`, `/64`,
    /// and `/96`. [`Proxy::start`](crate::Proxy::start) rejects other lengths.
    /// The well-known `64:ff9b::/96` prefix is always recognized and does not
    /// need to be registered.
    pub fn with_nat64_prefix(mut self, prefix: Ipv6Net) -> Self {
        if !self.nat64_prefixes.contains(&prefix) {
            self.nat64_prefixes.push(prefix);
        }
        self
    }

    /// Set the absolute deadline for receiving a complete CONNECT header.
    ///
    /// [`Proxy::start`](crate::Proxy::start) rejects zero and durations that
    /// cannot be represented as a runtime deadline.
    pub fn with_header_timeout(mut self, timeout: Duration) -> Self {
        self.header_timeout = timeout;
        self
    }

    /// Set the post-cancellation interval during which the old identity remains
    /// revoking so the accept loop can drain already-queued sockets. Every
    /// socket observed during revocation restarts the full interval. After an
    /// apparently quiet interval, the listener owner drains its ready accept
    /// queue and rechecks the interval before certifying cleanup.
    ///
    /// [`Proxy::start`](crate::Proxy::start) rejects durations that cannot be
    /// represented as a runtime deadline.
    pub fn with_identity_reuse_quiet_period(mut self, period: Duration) -> Self {
        self.identity_reuse_quiet_period = period;
        self
    }

    /// Emit bounded denial events through a caller-owned bounded channel.
    ///
    /// Delivery uses nonblocking [`SyncSender::try_send`]. Events are limited
    /// process-wide to `max_events_per_second`, clamped to `1..=10_000`.
    /// Rate- or channel-suppressed events are counted on the next event that
    /// can be delivered. A disconnected receiver silently disables delivery.
    pub fn with_diagnostic_channel(
        mut self,
        sender: SyncSender<DiagnosticEvent>,
        max_events_per_second: u32,
    ) -> Self {
        self.diagnostics = Some(DiagnosticConfig {
            sender,
            max_events_per_second: max_events_per_second.clamp(1, 10_000),
        });
        self
    }
}

const fn is_concrete_unicast(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(address) => is_unicast_v4(address),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(address) => is_unicast_v4(address),
            None => !address.is_unspecified() && !address.is_multicast(),
        },
    }
}

const fn is_bindable_listener(address: SocketAddr) -> bool {
    match address {
        SocketAddr::V4(address) => is_bindable_v4(*address.ip()),
        SocketAddr::V6(address) => match address.ip().to_ipv4_mapped() {
            Some(address) => is_bindable_v4(address),
            None => {
                !address.ip().is_multicast()
                    && (!is_scoped_unicast(*address.ip()) || address.scope_id() != 0)
            }
        },
    }
}

const fn is_bindable_v4(address: Ipv4Addr) -> bool {
    address.is_unspecified() || is_unicast_v4(address)
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            max_connections: 256,
            connection_attempt_rate: None,
            max_concurrent_dns: 32,
            max_concurrent_dials: 256,
            dns_cache_entries: 0,
            dns_cache_max_ttl: Duration::ZERO,
            dns_servers: Vec::new(),
            max_resolved_addresses: 64,
            max_header_bytes: 32 * 1_024,
            max_client_hello_bytes: 64 * 1_024,
            nat64_prefixes: Vec::new(),
            header_timeout: Duration::from_secs(10),
            identity_reuse_quiet_period: Duration::from_millis(25),
            diagnostics: None,
            upstream_proxy: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unrepresentable_runtime_durations() {
        assert!(
            ProxyConfig::default()
                .with_header_timeout(Duration::MAX)
                .validate()
                .is_err()
        );
        assert!(
            ProxyConfig::default()
                .with_identity_reuse_quiet_period(Duration::MAX)
                .validate()
                .is_err()
        );
        assert!(matches!(
            crate::Proxy::start(ProxyConfig::default().with_header_timeout(Duration::MAX)),
            Err(crate::ProxyError::Initialization(_))
        ));
    }

    #[test]
    fn rejects_a_zero_header_deadline() {
        assert!(matches!(
            crate::Proxy::start(ProxyConfig::default().with_header_timeout(Duration::ZERO)),
            Err(crate::ProxyError::Initialization(_))
        ));
    }

    #[test]
    fn rejects_zero_connection_attempt_rate_or_burst() {
        for config in [
            ProxyConfig::default().with_connection_attempt_rate(0, 1),
            ProxyConfig::default().with_connection_attempt_rate(1, 0),
        ] {
            assert!(matches!(
                crate::Proxy::start(config),
                Err(crate::ProxyError::Initialization(_))
            ));
        }
    }

    #[test]
    fn parser_and_resolver_ceilings_stay_bounded() {
        assert_eq!(
            ProxyConfig::default()
                .with_max_resolved_addresses(0)
                .max_resolved_addresses,
            1
        );
        assert_eq!(
            ProxyConfig::default()
                .with_max_resolved_addresses(usize::MAX)
                .max_resolved_addresses,
            1_024
        );
        assert_eq!(
            ProxyConfig::default()
                .with_max_client_hello_bytes(0)
                .max_client_hello_bytes,
            1_024
        );
        assert_eq!(
            ProxyConfig::default()
                .with_max_client_hello_bytes(usize::MAX)
                .max_client_hello_bytes,
            1024 * 1024
        );
    }

    #[test]
    fn dns_defaults_limit_transient_and_retained_work() {
        let defaults = ProxyConfig::default();
        assert_eq!(defaults.max_concurrent_dns, 32);
        assert_eq!(defaults.dns_cache_entries, 0);
        assert_eq!(defaults.dns_cache_max_ttl, Duration::ZERO);
    }

    #[test]
    fn default_global_connection_capacity_is_conservative() {
        assert_eq!(ProxyConfig::default().max_connections, 256);
    }

    #[test]
    fn resolver_cache_configuration_can_only_narrow_process_bounds() {
        let disabled = ProxyConfig::default().with_dns_cache(0, Duration::ZERO);
        assert_eq!(disabled.dns_cache_entries, 0);
        assert_eq!(disabled.dns_cache_max_ttl, Duration::ZERO);

        let clamped = ProxyConfig::default().with_dns_cache(u64::MAX, Duration::MAX);
        assert_eq!(clamped.dns_cache_entries, 64);
        assert_eq!(clamped.dns_cache_max_ttl, Duration::from_secs(86_400));
    }

    #[test]
    fn explicit_dns_servers_are_deduplicated_and_bounded() {
        let first = "127.0.0.1:53".parse().expect("valid DNS server");
        let mut config = ProxyConfig::default()
            .with_dns_server(first)
            .with_dns_server(first);
        assert_eq!(config.dns_servers, vec![first]);

        let server_limit = u16::try_from(MAX_DNS_SERVERS).expect("small DNS server ceiling");
        for port in 54..=(53 + server_limit) {
            config = config.with_dns_server(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port));
        }
        assert!(config.validate().is_err());
        assert!(matches!(
            crate::Proxy::start(config),
            Err(crate::ProxyError::Initialization(_))
        ));
    }

    #[test]
    fn explicit_scoped_ipv6_dns_server_is_rejected() {
        let scoped = "fe80::1".parse().expect("valid link-local address");
        let server = SocketAddr::V6(std::net::SocketAddrV6::new(scoped, 53, 0, 7));
        assert!(
            ProxyConfig::default()
                .with_dns_server(server)
                .validate()
                .is_err()
        );
        assert!(
            ProxyConfig::default()
                .with_dns_server(SocketAddr::new(scoped.into(), 53))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn scoped_ipv6_upstream_proxy_requires_a_zone() {
        let scoped = "fe80::1".parse().expect("valid link-local address");
        let without_zone = SocketAddr::V6(std::net::SocketAddrV6::new(scoped, 3128, 0, 0));
        let with_zone = SocketAddr::V6(std::net::SocketAddrV6::new(scoped, 3128, 0, 7));

        assert!(
            ProxyConfig::default()
                .with_upstream_proxy(without_zone)
                .validate()
                .is_err()
        );
        assert!(
            ProxyConfig::default()
                .with_upstream_proxy(with_zone)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn explicit_dns_server_requires_a_destination_port() {
        let server = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        assert!(
            ProxyConfig::default()
                .with_dns_server(server)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn upstream_proxy_requires_a_destination_port() {
        let proxy = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        assert!(
            ProxyConfig::default()
                .with_upstream_proxy(proxy)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn configured_remote_services_require_concrete_unicast_addresses() {
        for address in [
            "0.0.0.0:53".parse().expect("unspecified IPv4"),
            "[::]:53".parse().expect("unspecified IPv6"),
            "224.0.0.1:53".parse().expect("multicast IPv4"),
            "[ff02::1]:53".parse().expect("multicast IPv6"),
            "255.255.255.255:53".parse().expect("broadcast IPv4"),
            "0.0.0.1:53".parse().expect("this-network IPv4"),
            "240.0.0.1:53".parse().expect("reserved IPv4"),
        ] {
            assert!(
                ProxyConfig::default()
                    .with_dns_server(address)
                    .validate()
                    .is_err(),
                "accepted DNS server {address}"
            );
            assert!(
                ProxyConfig::default()
                    .with_upstream_proxy(SocketAddr::new(address.ip(), 3128))
                    .validate()
                    .is_err(),
                "accepted upstream proxy {}",
                address.ip()
            );
        }
    }

    #[test]
    fn mapped_remote_services_follow_the_ipv4_unicast_boundary() {
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(0, 0, 0, 1),
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::new(240, 0, 0, 1),
            Ipv4Addr::BROADCAST,
        ] {
            let mapped = SocketAddr::new(IpAddr::V6(address.to_ipv6_mapped()), 53);
            assert!(
                ProxyConfig::default()
                    .with_dns_server(mapped)
                    .validate()
                    .is_err(),
                "accepted mapped DNS server {mapped}"
            );
            assert!(
                ProxyConfig::default()
                    .with_upstream_proxy(SocketAddr::new(mapped.ip(), 3128))
                    .validate()
                    .is_err(),
                "accepted mapped upstream proxy {}",
                mapped.ip()
            );
        }

        for address in [Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 0, 2, 1)] {
            let mapped = SocketAddr::new(IpAddr::V6(address.to_ipv6_mapped()), 53);
            assert!(
                ProxyConfig::default()
                    .with_dns_server(mapped)
                    .validate()
                    .is_ok(),
                "rejected mapped DNS server {mapped}"
            );
            assert!(
                ProxyConfig::default()
                    .with_upstream_proxy(SocketAddr::new(mapped.ip(), 3128))
                    .validate()
                    .is_ok(),
                "rejected mapped upstream proxy {}",
                mapped.ip()
            );
        }
    }

    #[test]
    fn listener_requires_a_bindable_unicast_or_wildcard_address() {
        for address in [
            "224.0.0.1:0".parse().expect("multicast IPv4"),
            "[ff02::1]:0".parse().expect("multicast IPv6"),
            "255.255.255.255:0".parse().expect("broadcast IPv4"),
            "0.0.0.1:0".parse().expect("this-network IPv4"),
            "240.0.0.1:0".parse().expect("reserved IPv4"),
            "[fe80::1]:0".parse().expect("unscoped link-local IPv6"),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::new(0, 0, 0, 1).to_ipv6_mapped()), 0),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::new(224, 0, 0, 1).to_ipv6_mapped()), 0),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::new(240, 0, 0, 1).to_ipv6_mapped()), 0),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::BROADCAST.to_ipv6_mapped()), 0),
        ] {
            assert!(
                ProxyConfig::default()
                    .with_bind_address(address)
                    .validate()
                    .is_err(),
                "accepted listener {address}"
            );
        }

        for address in [
            "0.0.0.0:0".parse().expect("wildcard IPv4"),
            "[::]:0".parse().expect("wildcard IPv6"),
            "127.0.0.1:0".parse().expect("unicast IPv4"),
            "[::1]:0".parse().expect("unicast IPv6"),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped()), 0),
            SocketAddr::new(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()), 0),
            SocketAddr::V6(std::net::SocketAddrV6::new(
                "fe80::1".parse().expect("link-local IPv6"),
                0,
                0,
                7,
            )),
        ] {
            assert!(
                ProxyConfig::default()
                    .with_bind_address(address)
                    .validate()
                    .is_ok(),
                "rejected listener {address}"
            );
        }
    }

    #[test]
    fn diagnostic_rate_stays_bounded() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        let config = ProxyConfig::default().with_diagnostic_channel(sender.clone(), 0);
        assert_eq!(
            config
                .diagnostics
                .expect("diagnostics configured")
                .max_events_per_second,
            1
        );
        let config = ProxyConfig::default().with_diagnostic_channel(sender, u32::MAX);
        assert_eq!(
            config
                .diagnostics
                .expect("diagnostics configured")
                .max_events_per_second,
            10_000
        );
    }

    #[test]
    fn rejects_a_nonstandard_nat64_prefix_length() {
        let config = ProxyConfig::default().with_nat64_prefix(
            "2600:1f18:abcd:1200::/56"
                .parse()
                .expect("valid RFC 6052 prefix"),
        );
        config.validate().expect("RFC 6052 length is valid");

        let config = ProxyConfig::default().with_nat64_prefix(
            "2600:1f18:abcd:1234::/80"
                .parse()
                .expect("syntactically valid IPv6 prefix"),
        );
        assert!(config.validate().is_err());
        assert!(matches!(
            crate::Proxy::start(config),
            Err(crate::ProxyError::Initialization(_))
        ));
    }

    #[test]
    fn runtime_semaphore_limits_stay_within_the_runtime_bound() {
        let config = ProxyConfig::default()
            .with_max_connections(usize::MAX)
            .with_max_concurrent_dns(usize::MAX)
            .with_max_concurrent_dials(usize::MAX);
        assert_eq!(config.max_connections, tokio::sync::Semaphore::MAX_PERMITS);
        assert_eq!(
            config.max_concurrent_dns,
            tokio::sync::Semaphore::MAX_PERMITS
        );
        assert_eq!(
            config.max_concurrent_dials,
            tokio::sync::Semaphore::MAX_PERMITS
        );
        crate::Proxy::start(config)
            .expect("clamped global limit starts")
            .shutdown(Instant::now() + Duration::from_secs(1))
            .expect("proxy shutdown");
    }
}
