//! Concurrency-focused revocation tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use sandbox_egress::{AttachError, PeerIdentity, Policy, Proxy, ProxyConfig};

fn assert_terminal_read(result: std::io::Result<usize>) {
    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ) => {}
        result => panic!("capacity-rejected socket was not terminal: {result:?}"),
    }
}

fn saturated_usage(global_limit: usize, lease_limit: usize) -> sandbox_egress::FinalUsage {
    let proxy = Proxy::start(ProxyConfig::default().with_max_connections(global_limit))
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .max_connections(lease_limit)
                .expect("positive lease limit")
                .build()
                .expect("valid policy"),
        )
        .expect("attach lease");
    let endpoint = lease.endpoint().socket_addr();
    let mut occupying = TcpStream::connect(endpoint).expect("connect occupying client");
    occupying.write_all(b"CONNECT slow").expect("hold header");
    let deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().active_connections != 1 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(lease.usage().active_connections, 1);

    let mut rejected = TcpStream::connect(endpoint).expect("connect rejected client");
    rejected
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set rejection timeout");
    rejected.write_all(b"CONNECT rejected").ok();
    let mut byte = [0_u8; 1];
    assert_terminal_read(rejected.read(&mut byte));

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close saturated lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
    usage
}

#[test]
fn per_lease_capacity_is_reserved_and_accounted_before_spawn() {
    let usage = saturated_usage(2, 1).usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
}

#[test]
fn global_capacity_is_reserved_and_accounted_before_spawn() {
    let usage = saturated_usage(1, 2).usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
}

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

#[test]
fn mapped_ipv4_spelling_cannot_attach_a_second_policy() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach IPv4 spelling");

    assert!(matches!(
        proxy.attach(
            PeerIdentity::SourceIp(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped())),
            Policy::builder().build().expect("valid policy"),
        ),
        Err(AttachError::IdentityInUse)
    ));

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn successful_close_releases_identity_for_a_new_lease() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let lease = proxy
        .attach(
            identity.clone(),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("first attach");

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close first lease");
    let replacement = proxy
        .attach(identity, Policy::builder().build().expect("valid policy"))
        .expect("attach replacement");
    replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close replacement");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
