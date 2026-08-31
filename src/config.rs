use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Process-wide proxy configuration.
#[derive(Clone, Debug)]
#[must_use]
pub struct ProxyConfig {
    pub(crate) bind_address: SocketAddr,
    pub(crate) max_connections: usize,
    pub(crate) max_concurrent_dns: usize,
    pub(crate) max_header_bytes: usize,
    pub(crate) max_client_hello_bytes: usize,
    pub(crate) header_timeout: Duration,
    pub(crate) identity_reuse_quiet_period: Duration,
}

impl ProxyConfig {
    /// Set the listener address. Port zero asks the operating system to choose.
    pub fn with_bind_address(mut self, address: SocketAddr) -> Self {
        self.bind_address = address;
        self
    }

    /// Set the process-wide concurrent connection ceiling.
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max.max(1);
        self
    }

    /// Set the process-wide ceiling for DNS lookups executing concurrently.
    /// Connections waiting for a permit remain subject to their DNS and
    /// absolute handshake deadlines.
    pub fn with_max_concurrent_dns(mut self, max: usize) -> Self {
        self.max_concurrent_dns = max.max(1);
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

    /// Set the absolute deadline for receiving a complete CONNECT header.
    pub fn with_header_timeout(mut self, timeout: Duration) -> Self {
        self.header_timeout = timeout;
        self
    }

    /// Set the post-cancellation interval during which the old identity remains
    /// revoking so the accept loop can drain already-queued sockets.
    pub fn with_identity_reuse_quiet_period(mut self, period: Duration) -> Self {
        self.identity_reuse_quiet_period = period;
        self
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            max_connections: 1_024,
            max_concurrent_dns: 128,
            max_header_bytes: 32 * 1_024,
            max_client_hello_bytes: 64 * 1_024,
            header_timeout: Duration::from_secs(10),
            identity_reuse_quiet_period: Duration::from_millis(25),
        }
    }
}
