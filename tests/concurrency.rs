//! Concurrency-focused revocation tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Barrier, mpsc};
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
fn global_capacity_rejection_is_attributed_and_retry_recovers() {
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_bind_address(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
            .with_max_connections(1),
    )
    .expect("start dual-stack proxy");
    let ipv4_lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid IPv4 policy"),
        )
        .expect("attach IPv4 lease");
    let ipv6_lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            Policy::builder().build().expect("valid IPv6 policy"),
        )
        .expect("attach IPv6 lease");
    let port = proxy.endpoint().socket_addr().port();
    let mut occupying = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect IPv4");
    occupying.write_all(b"CONNECT slow").expect("hold permit");
    let admission_deadline = Instant::now() + Duration::from_secs(1);
    while ipv4_lease.usage().active_connections != 1 && Instant::now() < admission_deadline {
        thread::yield_now();
    }
    assert_eq!(ipv4_lease.usage().active_connections, 1);

    let mut rejected = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).expect("connect IPv6");
    rejected
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set rejection timeout");
    rejected.write_all(b"CONNECT queued").ok();
    let mut byte = [0_u8; 1];
    assert_terminal_read(rejected.read(&mut byte));
    assert_eq!(ipv4_lease.usage().denied_connections, 0);
    assert_eq!(ipv6_lease.usage().accepted_connections, 0);
    assert_eq!(ipv6_lease.usage().denied_connections, 1);

    let ipv4_usage = ipv4_lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("release occupied global permit")
        .usage();
    assert_eq!(ipv4_usage.accepted_connections, 1);
    assert_eq!(ipv4_usage.denied_connections, 0);
    assert_eq!(ipv4_usage.active_connections, 0);
    drop(occupying);

    let mut retry = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).expect("retry IPv6");
    retry.write_all(b"CONNECT slow").expect("hold retry header");
    let retry_deadline = Instant::now() + Duration::from_secs(1);
    while ipv6_lease.usage().active_connections != 1 && Instant::now() < retry_deadline {
        thread::yield_now();
    }
    assert_eq!(ipv6_lease.usage().active_connections, 1);
    let ipv6_usage = ipv6_lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close retried lease")
        .usage();
    assert_eq!(ipv6_usage.accepted_connections, 1);
    assert_eq!(ipv6_usage.denied_connections, 1);
    assert_eq!(ipv6_usage.active_connections, 0);
    drop(retry);

    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn full_diagnostic_channel_cannot_block_concurrent_denials_or_close() {
    const CLIENTS: usize = 64;

    let (diagnostic_tx, diagnostic_rx) = mpsc::sync_channel(0);
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_max_connections(CLIENTS)
            .with_diagnostic_channel(
                diagnostic_tx,
                u32::try_from(CLIENTS).expect("client count fits diagnostic rate"),
            ),
    )
    .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_port(443)
                .max_connections(CLIENTS)
                .expect("positive lease limit")
                .build()
                .expect("valid policy"),
        )
        .expect("attach lease");
    let endpoint = lease.endpoint().socket_addr();
    let barrier = Arc::new(Barrier::new(CLIENTS));
    let mut clients = Vec::with_capacity(CLIENTS);

    for _ in 0..CLIENTS {
        let barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(endpoint).expect("connect proxy");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set denial timeout");
            barrier.wait();
            stream
                .write_all(b"CONNECT denied.test:443 HTTP/1.1\r\nHost: denied.test\r\n\r\n")
                .expect("write denied request");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("read denied response");
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
            assert!(response.contains("host-denied"), "{response}");
        }));
    }
    for client in clients {
        client.join().expect("denied client thread");
    }

    let close_started = Instant::now();
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("close after diagnostic backpressure")
        .usage();
    assert!(close_started.elapsed() < Duration::from_secs(2));
    assert_eq!(usage.accepted_connections, CLIENTS as u64);
    assert_eq!(usage.denied_connections, CLIENTS as u64);
    assert_eq!(usage.completed_connections, 0);
    assert_eq!(usage.active_connections, 0);
    assert!(matches!(
        diagnostic_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
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
fn exactly_one_concurrent_attach_owns_an_identity() {
    const CONTENDERS: usize = 32;

    let proxy = Arc::new(Proxy::start(ProxyConfig::default()).expect("start proxy"));
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let barrier = Arc::new(Barrier::new(CONTENDERS));
    let mut contenders = Vec::with_capacity(CONTENDERS);

    for _ in 0..CONTENDERS {
        let proxy = Arc::clone(&proxy);
        let identity = identity.clone();
        let barrier = Arc::clone(&barrier);
        contenders.push(thread::spawn(move || {
            barrier.wait();
            proxy.attach(identity, Policy::builder().build().expect("valid policy"))
        }));
    }

    let mut winner = None;
    let mut rejected = 0;
    for contender in contenders {
        match contender.join().expect("attach contender") {
            Ok(lease) => assert!(winner.replace(lease).is_none(), "multiple leases won"),
            Err(AttachError::IdentityInUse) => rejected += 1,
            Err(error) => panic!("unexpected attach result: {error}"),
        }
    }

    assert_eq!(rejected, CONTENDERS - 1);
    let lease = winner.expect("one attach must win");
    assert!(matches!(
        proxy.attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        ),
        Err(AttachError::IdentityInUse)
    ));
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close winning lease");

    Arc::into_inner(proxy)
        .expect("all proxy references returned")
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
