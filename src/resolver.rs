//! Process-wide resolver construction and the narrow lookup boundary used by
//! the proxy data path.

#[cfg(test)]
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::sync::Arc;

use hickory_resolver::TokioResolver;

use crate::ProxyConfig;

pub(crate) enum ResolverBackend {
    System(Box<TokioResolver>),
    #[cfg(test)]
    Test(Arc<dyn TestResolver>),
}

pub(crate) fn build_system_resolver(config: &ProxyConfig) -> Result<TokioResolver, String> {
    let mut builder = if config.dns_servers.is_empty() {
        TokioResolver::builder_tokio().map_err(|error| error.to_string())?
    } else {
        let name_servers = config
            .dns_servers
            .iter()
            .copied()
            .map(configured_name_server)
            .collect();
        let resolver_config =
            hickory_resolver::config::ResolverConfig::from_parts(None, Vec::new(), name_servers);
        let mut builder = TokioResolver::builder_with_config(
            resolver_config,
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        );
        builder.options_mut().use_hosts_file = hickory_resolver::config::ResolveHosts::Never;
        builder
    };
    apply_resolver_cache_options(builder.options_mut(), config);
    builder.build().map_err(|error| error.to_string())
}

fn configured_name_server(address: SocketAddr) -> hickory_resolver::config::NameServerConfig {
    let mut udp = hickory_resolver::config::ConnectionConfig::udp();
    udp.port = address.port();
    let mut tcp = hickory_resolver::config::ConnectionConfig::tcp();
    tcp.port = address.port();
    hickory_resolver::config::NameServerConfig::new(address.ip(), true, vec![udp, tcp])
}

pub(crate) fn apply_resolver_cache_options(
    options: &mut hickory_resolver::config::ResolverOpts,
    config: &ProxyConfig,
) {
    options.cache_size = config.dns_cache_entries;
    options.positive_max_ttl = Some(config.dns_cache_max_ttl);
    options.negative_max_ttl = Some(config.dns_cache_max_ttl);
    options.try_tcp_on_error = true;
}

impl ResolverBackend {
    pub(crate) async fn lookup(
        &self,
        hostname: &str,
        max_addresses: usize,
    ) -> io::Result<Vec<IpAddr>> {
        let absolute_hostname = format!("{hostname}.");
        match self {
            Self::System(resolver) => resolver
                .lookup_ip(&absolute_hostname)
                .await
                .map(|lookup| {
                    lookup
                        .iter()
                        .take(max_addresses.saturating_add(1))
                        .collect()
                })
                .map_err(io::Error::other),
            #[cfg(test)]
            Self::Test(resolver) => resolver.lookup(&absolute_hostname).await,
        }
    }
}

#[cfg(test)]
pub(crate) trait TestResolver: Send + Sync {
    fn lookup<'a>(
        &'a self,
        hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>>;
}
