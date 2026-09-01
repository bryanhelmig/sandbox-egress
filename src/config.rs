use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use crate::DiagnosticEvent;
use crate::diagnostic::DiagnosticConfig;

/// Process-wide proxy configuration.
#[derive(Clone, Debug)]
#[must_use]
pub struct ProxyConfig {
    pub(crate) bind_address: SocketAddr,
    pub(crate) max_connections: usize,
    pub(crate) max_concurrent_dns: usize,
    pub(crate) max_resolved_addresses: usize,
    pub(crate) max_header_bytes: usize,
    pub(crate) max_client_hello_bytes: usize,
    pub(crate) header_timeout: Duration,
    pub(crate) identity_reuse_quiet_period: Duration,
    pub(crate) diagnostics: Option<DiagnosticConfig>,
}

impl ProxyConfig {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let now = Instant::now();
        if now.checked_add(self.header_timeout).is_none() {
            return Err("header timeout is too large");
        }
        if now.checked_add(self.identity_reuse_quiet_period).is_none() {
            return Err("identity reuse quiet period is too large");
        }
        Ok(())
    }

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

    /// Set the absolute deadline for receiving a complete CONNECT header.
    ///
    /// [`Proxy::start`](crate::Proxy::start) rejects durations that cannot be
    /// represented as a runtime deadline.
    pub fn with_header_timeout(mut self, timeout: Duration) -> Self {
        self.header_timeout = timeout;
        self
    }

    /// Set the post-cancellation interval during which the old identity remains
    /// revoking so the accept loop can drain already-queued sockets.
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
            max_resolved_addresses: 64,
            max_header_bytes: 32 * 1_024,
            max_client_hello_bytes: 64 * 1_024,
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
}
