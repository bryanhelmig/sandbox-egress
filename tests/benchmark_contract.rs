//! A faster denial must never satisfy the allowed-CONNECT benchmark oracle.

#[path = "../benches/support/mod.rs"]
mod support;

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::time::{Duration, Instant};

#[test]
fn allowed_benchmark_oracle_rejects_a_real_policy_denial() {
    let proxy = Proxy::start(ProxyConfig::default()).unwrap();
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(Ipv4Addr::LOCALHOST.into()),
            Policy::builder().allow_port(443).build().unwrap(),
        )
        .unwrap();
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .write_all(b"CONNECT denied.invalid:443 HTTP/1.1\r\nHost: denied.invalid\r\n\r\n")
        .unwrap();
    let mut response = [0; 39];
    client.read_exact(&mut response).unwrap();
    assert!(response.starts_with(b"HTTP/1.1 403"));
    assert!(std::panic::catch_unwind(|| support::assert_connect_success(&response)).is_err());
    support::assert_connect_success(b"HTTP/1.1 200 Connection Established\r\n\r\n");
    drop(client);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .unwrap()
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .unwrap();
}
