use super::*;

#[test]
fn dial_concurrency_is_bounded_and_queued_attempts_cancel_on_close() {
    const CLIENTS: usize = 5;
    const DIAL_LIMIT: usize = 2;

    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(PendingConnector {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default()
            .with_max_connections(CLIENTS)
            .with_max_concurrent_dials(DIAL_LIMIT),
        resolver,
        connector,
    )
    .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("pending.test", 443),
        )
        .expect("attach lease");

    let mut clients = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
        )
        .expect("write CONNECT");
        clients.push(client);
    }
    for _ in 0..DIAL_LIMIT {
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dial entered");
    }
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(active.load(Ordering::Acquire), DIAL_LIMIT);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close dial-bound lease")
        .usage();
    assert_eq!(final_usage.accepted_connections, CLIENTS as u64);
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.denied_connections, 0);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    for client in clients {
        assert_client_stopped(client);
    }
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dial_permit_deadline_has_a_distinct_denial() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(PendingConnector {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default()
            .with_bind_address("[::]:0".parse().expect("valid dual-stack address"))
            .with_max_connections(2)
            .with_max_concurrent_dials(1),
        resolver,
        connector,
    )
    .expect("start dual-stack proxy");
    let holding_lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("pending.test", 443),
        )
        .expect("attach permit-holding lease");
    let waiting_policy = Policy::builder()
        .allow_host("pending.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback network"))
        .allow_port(443)
        .dns_timeout(Duration::from_millis(100))
        .handshake_timeout(Duration::from_millis(100))
        .build()
        .expect("valid waiting policy");
    let waiting_lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
            waiting_policy,
        )
        .expect("attach permit-waiting lease");
    let port = proxy.endpoint().socket_addr().port();

    let mut holding_client = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .expect("connect permit holder");
    std::io::Write::write_all(
        &mut holding_client,
        b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
    )
    .expect("write permit-holding CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("holding dial entered");

    let mut waiting_client = std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port))
        .expect("connect permit waiter");
    std::io::Write::write_all(
        &mut waiting_client,
        b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
    )
    .expect("write permit-waiting CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut waiting_client, &mut response)
        .expect("read dial-capacity denial");

    assert!(
        response.is_empty(),
        "expired denial wrote bytes: {response}"
    );
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(active.load(Ordering::Acquire), 1);
    let waiting_usage = waiting_lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close permit waiter")
        .usage();
    assert_eq!(waiting_usage.denied_connections, 1);
    holding_lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close permit holder");
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_client_stopped(holding_client);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
