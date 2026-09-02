//! Concurrency-focused revocation tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
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

fn rate_limited_usage(
    global: bool,
) -> (
    sandbox_egress::FinalUsage,
    sandbox_egress::DiagnosticEvent,
    u64,
) {
    let (diagnostic_tx, diagnostic_rx) = mpsc::sync_channel(1);
    let config = if global {
        ProxyConfig::default().with_connection_attempt_rate(1, 1)
    } else {
        ProxyConfig::default()
    }
    .with_diagnostic_channel(diagnostic_tx, 10);
    let proxy = Proxy::start(config).expect("start proxy");
    let mut policy = Policy::builder()
        .max_connections(2)
        .expect("positive lease limit");
    if !global {
        policy = policy
            .connection_attempt_rate(1, 1)
            .expect("positive attempt rate and burst");
    }
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy.build().expect("valid policy"),
        )
        .expect("attach lease");
    let lease_id = lease.id();
    let endpoint = lease.endpoint().socket_addr();

    let mut admitted = TcpStream::connect(endpoint).expect("connect admitted client");
    admitted.write_all(b"CONNECT slow").expect("hold header");
    let deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().active_connections != 1 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(lease.usage().active_connections, 1);

    let mut rejected = TcpStream::connect(endpoint).expect("connect rate-limited client");
    rejected
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set rejection timeout");
    rejected.write_all(b"CONNECT rejected").ok();
    let mut byte = [0_u8; 1];
    assert_terminal_read(rejected.read(&mut byte));
    let diagnostic = diagnostic_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("rate-limit diagnostic");

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close rate-limited lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
    (usage, diagnostic, lease_id)
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
fn per_lease_connection_attempt_rate_is_reserved_before_spawn() {
    let (final_usage, diagnostic, lease_id) = rate_limited_usage(false);
    let usage = final_usage.usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    assert_eq!(diagnostic.lease_id, lease_id);
    assert_eq!(diagnostic.reason.as_str(), "lease-rate");
}

#[test]
fn global_connection_attempt_rate_is_reserved_before_spawn() {
    let (final_usage, diagnostic, lease_id) = rate_limited_usage(true);
    let usage = final_usage.usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    assert_eq!(diagnostic.lease_id, lease_id);
    assert_eq!(diagnostic.reason.as_str(), "global-rate");
}

#[test]
fn replacement_lease_starts_with_fresh_connection_attempt_burst() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let policy = || {
        Policy::builder()
            .connection_attempt_rate(1, 1)
            .expect("positive attempt rate and burst")
            .build()
            .expect("valid policy")
    };
    let old = proxy
        .attach(identity.clone(), policy())
        .expect("attach old lease");
    let endpoint = old.endpoint().socket_addr();
    let mut old_client = TcpStream::connect(endpoint).expect("connect old client");
    old_client
        .write_all(b"CONNECT old")
        .expect("hold old header");
    let old_deadline = Instant::now() + Duration::from_secs(1);
    while old.usage().active_connections != 1 && Instant::now() < old_deadline {
        thread::yield_now();
    }
    assert_eq!(old.usage().active_connections, 1);
    old.close(Instant::now() + Duration::from_secs(1))
        .expect("certify old lease");
    drop(old_client);

    let replacement = proxy
        .attach(identity, policy())
        .expect("attach replacement lease");
    let mut replacement_client = TcpStream::connect(endpoint).expect("connect replacement client");
    replacement_client
        .write_all(b"CONNECT replacement")
        .expect("hold replacement header");
    let replacement_deadline = Instant::now() + Duration::from_secs(1);
    while replacement.usage().active_connections != 1 && Instant::now() < replacement_deadline {
        thread::yield_now();
    }
    assert_eq!(replacement.usage().active_connections, 1);
    let usage = replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("certify replacement lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 0);
    drop(replacement_client);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn replacement_lease_does_not_reset_global_connection_attempt_bucket() {
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_connection_attempt_rate(1, 1)
            .with_identity_reuse_quiet_period(Duration::ZERO),
    )
    .expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let old = proxy
        .attach(
            identity.clone(),
            Policy::builder().build().expect("valid old policy"),
        )
        .expect("attach old lease");
    let endpoint = old.endpoint().socket_addr();
    let mut old_client = TcpStream::connect(endpoint).expect("connect old client");
    old_client
        .write_all(b"CONNECT old")
        .expect("hold old header");
    let admission_deadline = Instant::now() + Duration::from_secs(1);
    while old.usage().active_connections != 1 && Instant::now() < admission_deadline {
        thread::yield_now();
    }
    assert_eq!(old.usage().active_connections, 1);
    old.close(Instant::now() + Duration::from_secs(1))
        .expect("certify old lease");
    drop(old_client);

    let replacement = proxy
        .attach(
            identity,
            Policy::builder().build().expect("valid replacement policy"),
        )
        .expect("attach replacement lease");
    let mut rejected = TcpStream::connect(endpoint).expect("connect replacement client");
    rejected
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set rejection timeout");
    rejected.write_all(b"CONNECT replacement").ok();
    let mut byte = [0_u8; 1];
    assert_terminal_read(rejected.read(&mut byte));
    let accounting_deadline = Instant::now() + Duration::from_secs(1);
    while replacement.usage().denied_connections != 1 && Instant::now() < accounting_deadline {
        thread::yield_now();
    }
    let usage = replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("certify replacement lease")
        .usage();
    assert_eq!(usage.accepted_connections, 0);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn impossible_source_identities_fail_before_consuming_a_lease_id() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    for address in [
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
        IpAddr::V6("ff02::1".parse().expect("IPv6 multicast")),
        IpAddr::V6(Ipv4Addr::new(224, 0, 0, 1).to_ipv6_mapped()),
        IpAddr::V4(Ipv4Addr::BROADCAST),
        IpAddr::V6(Ipv4Addr::BROADCAST.to_ipv6_mapped()),
    ] {
        assert!(matches!(
            proxy.attach(
                PeerIdentity::SourceIp(address),
                Policy::builder().build().expect("valid policy"),
            ),
            Err(AttachError::InvalidIdentity)
        ));
    }

    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect("attach concrete source identity");
    assert_eq!(lease.id(), 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
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
    let accounting_deadline = Instant::now() + Duration::from_secs(1);
    while ipv6_lease.usage().denied_connections != 1 && Instant::now() < accounting_deadline {
        thread::yield_now();
    }
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
fn dial_permit_is_released_before_tunnelling() {
    const CONNECT_RESPONSE: &[u8; 39] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let upstream_port = upstream.local_addr().expect("upstream address").port();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut connections = Vec::with_capacity(2);
        for _ in 0..2 {
            let (connection, _) = upstream.accept().expect("accept upstream connection");
            connections.push(connection);
            accepted_tx.send(()).expect("report upstream connection");
        }
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release upstream connections");
    });

    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_identity_reuse_quiet_period(Duration::ZERO)
            .with_max_connections(2)
            .with_max_concurrent_dials(1),
    )
    .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_network("127.0.0.0/8".parse().expect("valid loopback network"))
                .allow_port(upstream_port)
                .max_connections(2)
                .expect("positive lease limit")
                .build()
                .expect("valid policy"),
        )
        .expect("attach lease");
    let endpoint = lease.endpoint().socket_addr();
    let mut clients = Vec::with_capacity(2);

    for _ in 0..2 {
        let mut client = TcpStream::connect(endpoint).expect("connect proxy");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set CONNECT response timeout");
        write!(
            client,
            "CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        )
        .expect("write CONNECT");
        let mut response = [0_u8; CONNECT_RESPONSE.len()];
        client
            .read_exact(&mut response)
            .expect("read CONNECT success");
        assert_eq!(&response, CONNECT_RESPONSE);
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe upstream dial");
        clients.push(client);
    }

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close two active tunnels")
        .usage();
    assert_eq!(final_usage.accepted_connections, 2);
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.completed_connections, 0);
    assert_eq!(final_usage.denied_connections, 0);
    drop(clients);
    release_tx.send(()).expect("release upstream server");
    server.join().expect("join upstream server");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upstream_proxy_negotiations_hold_the_dial_budget_until_success() {
    const CONNECTIONS: usize = 4;
    const DIAL_LIMIT: usize = 2;
    const UPSTREAM_CONNECT: &[u8] =
        b"CONNECT 127.0.0.2:443 HTTP/1.1\r\nHost: 127.0.0.2:443\r\n\r\n";

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream proxy");
    let upstream_proxy = listener.local_addr().expect("upstream proxy address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(DIAL_LIMIT);
    let (third_tx, third_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let server = thread::spawn(move || {
        let mut negotiations = Vec::with_capacity(DIAL_LIMIT);
        for _ in 0..DIAL_LIMIT {
            let (mut stream, _) = listener.accept().expect("accept upstream negotiation");
            let mut request = [0_u8; UPSTREAM_CONNECT.len()];
            stream
                .read_exact(&mut request)
                .expect("read upstream CONNECT");
            assert_eq!(&request, UPSTREAM_CONNECT);
            negotiations.push(stream);
            accepted_tx.send(()).expect("report upstream negotiation");
        }
        let (third, _) = listener.accept().expect("accept observer wakeup");
        let _ = third_tx.send(());
        release_rx.recv().expect("release upstream negotiations");
        drop(third);
        drop(negotiations);
    });

    let target: SocketAddr = "127.0.0.2:443".parse().expect("numeric target");
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_max_connections(CONNECTIONS)
            .with_max_concurrent_dials(DIAL_LIMIT)
            .with_upstream_proxy(upstream_proxy),
    )
    .expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse().expect("loopback CIDR"))
        .allow_port(target.port())
        .max_connections(CONNECTIONS)
        .expect("positive lease limit")
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {}\r\n\r\n", target.ip());
    let mut clients = Vec::with_capacity(CONNECTIONS);
    for _ in 0..CONNECTIONS {
        let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect guest");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set guest timeout");
        client
            .write_all(request.as_bytes())
            .expect("write guest CONNECT");
        clients.push(client);
    }
    for _ in 0..DIAL_LIMIT {
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe bounded upstream negotiation");
    }
    let active_deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().active_connections != CONNECTIONS as u64 && Instant::now() < active_deadline
    {
        thread::yield_now();
    }
    assert_eq!(lease.usage().active_connections, CONNECTIONS as u64);
    let third_seen = third_rx.recv_timeout(Duration::from_millis(200)).is_ok();

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("cancel active and queued negotiations")
        .usage();
    assert_eq!(usage.accepted_connections, CONNECTIONS as u64);
    assert_eq!(usage.denied_connections, 0);
    assert_eq!(usage.active_connections, 0);
    for mut client in clients {
        let mut byte = [0_u8; 1];
        assert_terminal_read(client.read(&mut byte));
    }
    if !third_seen {
        TcpStream::connect(upstream_proxy).expect("wake upstream observer");
    }
    release_tx.send(()).expect("release upstream observer");
    server.join().expect("join upstream proxy");
    assert!(!third_seen, "dial budget admitted a third negotiation");
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
