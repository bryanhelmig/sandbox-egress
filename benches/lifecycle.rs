//! Lifecycle microbenchmarks for regression detection.
#![allow(missing_docs)]

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use egress_lease::{PeerIdentity, Policy, Proxy, ProxyConfig};

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

criterion_group!(benches, attach_and_close);
criterion_main!(benches);
