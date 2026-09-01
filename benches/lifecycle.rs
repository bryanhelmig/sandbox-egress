//! Lifecycle microbenchmarks for regression detection.
#![allow(missing_docs)]

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use ipnet::Ipv6Net;
use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn attach_and_close(criterion: &mut Criterion) {
    let proxy =
        Proxy::start(ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO))
            .expect("start benchmark proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let policy = Policy::builder().build().expect("valid policy");

    criterion.bench_function("attach_close_empty_lease", |bencher| {
        bencher.iter(|| {
            let lease = proxy
                .attach(identity.clone(), policy.clone())
                .expect("attach benchmark lease");
            lease
                .close(Instant::now() + Duration::from_secs(1))
                .expect("close benchmark lease");
        });
    });

    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("shutdown benchmark proxy");
}

fn config_ownership(criterion: &mut Criterion) {
    let mut config = ProxyConfig::default();
    for port in 5_300..5_308 {
        config = config.with_dns_server(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port));
    }
    for prefix in [
        "2001:db8::/32",
        "2001:db8:100::/40",
        "2001:db8:200::/48",
        "2001:db8:300::/56",
        "2001:db8:400::/64",
        "2001:db8:500::/96",
    ] {
        config = config.with_nat64_prefix(prefix.parse::<Ipv6Net>().expect("valid NAT64 prefix"));
    }

    criterion.bench_function("clone_populated_proxy_config_control", |bencher| {
        bencher.iter(|| black_box(config.clone()));
    });
    let shared = Arc::new(config);
    criterion.bench_function("clone_shared_proxy_config", |bencher| {
        bencher.iter(|| black_box(Arc::clone(&shared)));
    });
}

criterion_group!(benches, attach_and_close, config_ownership);
criterion_main!(benches);
