//! Minimal executable wrapper around the embeddable library.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hosts: Vec<String> = std::env::args().skip(1).collect();
    if hosts.is_empty() {
        eprintln!("usage: sandbox-egress HOST [HOST ...]");
        std::process::exit(2);
    }

    let mut builder = Policy::builder().allow_port(443);
    for host in hosts {
        builder = builder.allow_host(host)?;
    }
    let policy = builder.build()?;
    let proxy = Proxy::start(ProxyConfig::default())?;
    let lease = proxy.attach(
        PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        policy,
    )?;

    println!("HTTPS_PROXY={}", lease.endpoint());
    println!("Press Enter to revoke the lease.");
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    let usage = lease
        .close(Instant::now() + Duration::from_secs(5))?
        .usage();
    println!("final usage: {usage:?}");
    proxy.shutdown(Instant::now() + Duration::from_secs(5))?;
    Ok(())
}
