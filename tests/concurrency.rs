//! Concurrency-focused revocation tests.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{PeerIdentity, Policy, Proxy, ProxyConfig};

#[test]
fn concurrent_slow_headers_are_all_owned_and_revoked() {
    const CLIENTS: usize = 32;
    let proxy = Proxy::start(ProxyConfig::default().with_max_connections(CLIENTS + 4))
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_host("example.com")
                .expect("valid host")
                .max_connections(CLIENTS)
                .expect("positive limit")
                .build()
                .expect("valid policy"),
        )
        .expect("attach lease");
    let endpoint = lease.endpoint().socket_addr();

    let mut clients = Vec::new();
    for _ in 0..CLIENTS {
        clients.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(endpoint).expect("connect proxy");
            stream
                .write_all(b"CONNECT example.com")
                .expect("slow header");
            thread::sleep(Duration::from_millis(300));
        }));
    }
    let wait_deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().accepted_connections < CLIENTS as u64 && Instant::now() < wait_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(lease.usage().accepted_connections, CLIENTS as u64);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("revoke all clients")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.accepted_connections, CLIENTS as u64);
    for client in clients {
        client.join().expect("client thread");
    }
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn identity_cannot_be_attached_twice() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let lease = proxy
        .attach(
            identity.clone(),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("first attach");
    assert!(
        proxy
            .attach(identity, Policy::builder().build().expect("valid policy"))
            .is_err()
    );
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
