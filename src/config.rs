use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use ipnet::Ipv6Net;

use crate::DiagnosticEvent;
use crate::diagnostic::DiagnosticConfig;

pub(crate) const MAX_DNS_CACHE_ENTRIES: u64 = 8_192;
pub(crate) const MAX_DNS_CACHE_TTL: Duration = Duration::from_secs(86_400);
pub(crate) const MAX_DNS_SERVERS: usize = 8;

/// Process-wide proxy configuration.
#[derive(Clone, Debug)]
#[must_use]
pub struct ProxyConfig {
    pub(crate) bind_address: SocketAddr,
    pub(crate) max_connections: usize,
    pub(crate) max_concurrent_dns: usize,
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
}

impl ProxyConfig {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.header_timeout.is_zero() {
            return Err("header timeout must be nonzero");
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
        if self.dns_servers.iter().any(|server| match server {
            SocketAddr::V4(_) => false,
            SocketAddr::V6(server) => server.scope_id() != 0,
        }) {
            return Err("scoped IPv6 DNS servers are not supported");
        }
        Ok(())
    }

    /// Set the listener address. Port zero asks the operating system to choose.
    pub fn with_bind_address(mut self, address: SocketAddr) -> Self {
        self.bind_address = address;
        self
    }

    /// Set the process-wide concurrent connection ceiling, clamped to the
    /// runtime semaphore's safe range.
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max.clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
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

    /// Bound the process-wide resolver cache by response count and TTL.
    ///
    /// The values can narrow but cannot exceed the crate's defaults of 8,192
    /// responses and 24 hours. The TTL ceiling applies to both successful and
    /// negative answers. Set `entries` to zero to disable the cache.
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
    /// rejects more than eight distinct servers and scoped IPv6 addresses.
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

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            max_connections: 1_024,
            max_concurrent_dns: 128,
            dns_cache_entries: MAX_DNS_CACHE_ENTRIES,
            dns_cache_max_ttl: MAX_DNS_CACHE_TTL,
            dns_servers: Vec::new(),
            max_resolved_addresses: 64,
            max_header_bytes: 32 * 1_024,
            max_client_hello_bytes: 64 * 1_024,
            nat64_prefixes: Vec::new(),
            header_timeout: Duration::from_secs(10),
            identity_reuse_quiet_period: Duration::from_millis(25),
            diagnostics: None,
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
    fn resolved_address_ceiling_stays_bounded() {
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
    }

    #[test]
    fn resolver_cache_configuration_can_only_narrow_process_bounds() {
        let disabled = ProxyConfig::default().with_dns_cache(0, Duration::ZERO);
        assert_eq!(disabled.dns_cache_entries, 0);
        assert_eq!(disabled.dns_cache_max_ttl, Duration::ZERO);

        let clamped = ProxyConfig::default().with_dns_cache(u64::MAX, Duration::MAX);
        assert_eq!(clamped.dns_cache_entries, 8_192);
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
        let server = SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().expect("valid link-local address"),
            53,
            0,
            7,
        ));
        assert!(
            ProxyConfig::default()
                .with_dns_server(server)
                .validate()
                .is_err()
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
            .with_max_concurrent_dns(usize::MAX);
        assert_eq!(config.max_connections, tokio::sync::Semaphore::MAX_PERMITS);
        assert_eq!(
            config.max_concurrent_dns,
            tokio::sync::Semaphore::MAX_PERMITS
        );
        crate::Proxy::start(config)
            .expect("clamped global limit starts")
            .shutdown(Instant::now() + Duration::from_secs(1))
            .expect("proxy shutdown");
    }
}
