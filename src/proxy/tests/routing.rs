use super::*;

#[test]
fn invalid_host_port_is_denied_before_dns_or_dial() {
    let (lookups, observed) = mpsc::channel();
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default(),
        Arc::new(CapturingAnswerResolver {
            captured: lookups,
            answers: vec!["192.0.2.1".parse().unwrap()],
        }),
        Arc::new(RejectingConnector(Arc::clone(&dials))),
    )
    .unwrap();
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(std::net::Ipv4Addr::LOCALHOST.into()),
            Policy::builder()
                .allow_host("allowed.test")
                .unwrap()
                .allow_network("0.0.0.0/0".parse().unwrap())
                .allow_network("::/0".parse().unwrap())
                .allow_port(443)
                .build()
                .unwrap(),
        )
        .unwrap();
    let mut count = 0;
    for host in ["allowed.test", "192.0.2.1", "[2001:db8::1]"] {
        for port in ["abc", "99999", "", "0", "444"] {
            let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr()).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            std::io::Write::write_all(
                &mut client,
                format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            assert!(read_blocking_header(&mut client).starts_with(b"HTTP/1.1 400"));
            count += 1;
        }
    }
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .unwrap()
        .usage();
    assert_eq!(usage.accepted_connections, count);
    assert_eq!(usage.denied_connections, count);
    assert_eq!(usage.active_connections, 0);
    assert!(observed.try_recv().is_err());
    assert_eq!(dials.load(Ordering::Acquire), 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .unwrap();
}

#[test]
fn pending_first_address_cannot_starve_a_reachable_second_address() {
    let target = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind fallback target");
    let target_address = target.local_addr().expect("fallback target address");
    let first = SocketAddr::new("192.0.2.1".parse().expect("first test IP"), 443);
    let second = SocketAddr::new("198.51.100.1".parse().expect("second test IP"), 443);
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let connector = ConnectorBackend::Test(Arc::new(PendingThenLoopbackConnector {
        pending: first,
        loopback: target_address,
        attempts: Arc::clone(&attempts),
    }));
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");

    let connected = runtime.block_on(dial_approved_addresses(
        vec![first, second],
        &connector,
        &CancellationToken::new(),
        TokioInstant::now() + Duration::from_millis(400),
    ));
    assert!(connected.is_some(), "reachable fallback was not attempted");
    assert_eq!(
        *attempts.lock().expect("attempt list poisoned"),
        vec![first, second]
    );
}

#[test]
fn revocation_before_refusal_prevents_a_fallback_dial() {
    let first = SocketAddr::new("192.0.2.1".parse().expect("first test IP"), 443);
    let second = SocketAddr::new("192.0.2.2".parse().expect("second test IP"), 443);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let connector = ConnectorBackend::Test(Arc::new(RefuseAfterReleaseConnector {
        first,
        entered: entered_tx,
        release: Mutex::new(Some(release_rx)),
        attempts: Arc::clone(&attempts),
    }));
    let cancellation = CancellationToken::new();
    let cancel_from_thread = cancellation.clone();
    let coordinator = thread::spawn(move || {
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe first attempt");
        cancel_from_thread.cancel();
        release_tx.send(()).expect("release first refusal");
    });
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");

    let connected = runtime.block_on(dial_approved_addresses(
        vec![first, second],
        &connector,
        &cancellation,
        TokioInstant::now() + Duration::from_secs(1),
    ));
    coordinator.join().expect("join revocation coordinator");
    assert!(connected.is_none());
    assert_eq!(
        *attempts.lock().expect("attempt list poisoned"),
        vec![first]
    );
}

#[test]
fn upstream_proxy_refusal_falls_back_without_another_lookup() {
    let first: SocketAddr = "192.0.2.1:443".parse().expect("first target");
    let second: SocketAddr = "192.0.2.2:443".parse().expect("second target");
    let (upstream_proxy, requests_rx, server) = start_refusing_upstream();

    let (hostname_tx, hostname_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingAnswerResolver {
        captured: hostname_tx,
        answers: vec![first.ip(), second.ip()],
    });
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default().with_upstream_proxy(upstream_proxy),
        resolver,
    )
    .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("fallback.test")
        .expect("valid hostname")
        .allow_network("192.0.2.0/24".parse().expect("test network"))
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach localhost");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect guest");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT fallback.test:443 HTTP/1.1\r\nHost: fallback.test\r\n\r\n",
    )
    .expect("write guest CONNECT");
    let mut response = [0_u8; 39];
    std::io::Read::read_exact(&mut client, &mut response).expect("read CONNECT success");
    assert_eq!(&response, CONNECT_SUCCESS_RESPONSE);
    let mut greeting = [0_u8; 5];
    std::io::Read::read_exact(&mut client, &mut greeting).expect("read greeting");
    assert_eq!(&greeting, b"hello");
    std::io::Write::write_all(&mut client, b"ping").expect("write tunnel upload");
    let mut pong = [0_u8; 4];
    std::io::Read::read_exact(&mut client, &mut pong).expect("read tunnel download");
    assert_eq!(&pong, b"pong");
    std::net::TcpStream::shutdown(&client, std::net::Shutdown::Write)
        .expect("finish tunnel upload");
    std::io::Read::read_to_end(&mut client, &mut Vec::new()).expect("read tunnel shutdown");
    server.join().expect("join upstream proxy");

    assert_eq!(
        hostname_rx.recv().expect("receive lookup"),
        "fallback.test."
    );
    assert!(hostname_rx.try_recv().is_err(), "unexpected second lookup");
    assert_eq!(
        requests_rx.recv().expect("receive CONNECT requests"),
        vec![
            format!("CONNECT {first} HTTP/1.1\r\nHost: {first}\r\n\r\n").into_bytes(),
            format!("CONNECT {second} HTTP/1.1\r\nHost: {second}\r\n\r\n").into_bytes(),
        ]
    );
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("certified close")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.completed_connections, 1);
    assert_eq!(usage.denied_connections, 0);
    assert_eq!(usage.uploaded_bytes, 4);
    assert_eq!(usage.downloaded_bytes, 9);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
}

#[test]
fn dns_concurrency_is_bounded_and_queued_lookups_cancel_on_close() {
    const CLIENTS: usize = 5;
    const DNS_LIMIT: usize = 2;

    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resolver = Arc::new(PendingResolver {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default()
            .with_max_connections(CLIENTS)
            .with_max_concurrent_dns(DNS_LIMIT),
        resolver,
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
    for _ in 0..DNS_LIMIT {
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lookup entered");
    }
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(active.load(Ordering::Acquire), DNS_LIMIT);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS-bound lease");
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    drop(clients);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_deadline_has_a_distinct_denial_and_never_dials() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resolver = Arc::new(PendingResolver {
        entered: entered_tx,
        active: Arc::clone(&active),
    });
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start DNS deadline proxy");
    let policy = Policy::builder()
        .allow_host("pending.test")
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(443)
        .dns_timeout(Duration::from_millis(20))
        .handshake_timeout(Duration::from_secs(1))
        .build()
        .expect("valid DNS deadline policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach DNS deadline lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT pending.test:443 HTTP/1.1\r\nHost: pending.test\r\n\r\n",
    )
    .expect("write CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lookup entered");

    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS timeout denial");
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(response.contains("dns-timeout"), "{response}");
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS deadline lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_terminal_results_remain_distinct_and_never_dial() {
    assert_dns_terminal_denial("failed.test", Arc::new(FailingResolver), "dns-failed");
    assert_dns_terminal_denial(
        "empty.test",
        Arc::new(FixedAnswerResolver(Vec::new())),
        "dns-empty",
    );
}

#[test]
fn late_dns_answer_cannot_dial_after_lease_close() {
    let target =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind target");
    target.set_nonblocking(true).expect("nonblocking target");
    let port = target.local_addr().expect("target address").port();
    let (started_tx, started_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    let resolver = Arc::new(LateAnswerResolver {
        started: started_tx,
        answer: Mutex::new(Some(answer_rx)),
    });
    let proxy =
        Proxy::start_with_test_resolver(ProxyConfig::default(), resolver).expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("late.test", port),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT late.test:{port} HTTP/1.1\r\nHost: late.test\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lookup entered");

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close resolving lease");
    assert!(
        answer_tx
            .send(vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)])
            .is_err(),
        "revocation must drop the late-answer receiver"
    );
    assert!(matches!(
        target.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn test_and_system_resolvers_receive_the_same_absolute_hostname() {
    let (hostname_tx, hostname_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingResolver(hostname_tx));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("mixed.case.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT Mixed.Case.Test.:443 HTTP/1.1\r\nHost: mixed.case.test\r\n\r\n",
    )
    .expect("write CONNECT");

    assert_eq!(
        hostname_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("resolver hostname"),
        "mixed.case.test."
    );
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn system_resolver_uses_the_configured_cache_bounds() {
    let config = ProxyConfig::default().with_dns_cache(17, Duration::from_secs(19));
    let resolver = build_system_resolver(&config).expect("build system resolver");
    assert_eq!(resolver.options().cache_size, 17);
    assert_eq!(
        resolver.options().positive_max_ttl,
        Some(Duration::from_secs(19))
    );
    assert_eq!(
        resolver.options().negative_max_ttl,
        Some(Duration::from_secs(19))
    );
    assert!(resolver.options().try_tcp_on_error);
}

#[test]
fn explicit_dns_server_bypasses_host_configuration() {
    let (address, server) = start_local_dns(1, local_a_response);
    let config = ProxyConfig::default().with_dns_server(address);
    let resolver = build_system_resolver(&config).expect("build explicit resolver");
    assert_eq!(
        resolver.options().use_hosts_file,
        hickory_resolver::config::ResolveHosts::Never
    );
    assert!(resolver.options().try_tcp_on_error);

    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");
    runtime.block_on(async {
        let answer = resolver
            .ipv4_lookup("explicit-resolver.test.")
            .await
            .expect("resolve through configured DNS server");
        assert_eq!(answer.answers().len(), 1);
        assert_eq!(answer.answers()[0].data.to_string(), "127.0.0.1");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn explicit_dns_server_retries_truncated_udp_over_tcp() {
    let (address, server) = start_truncated_udp_dns();
    let config = ProxyConfig::default().with_dns_server(address);
    let resolver = build_system_resolver(&config).expect("build explicit resolver");
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        let answer = resolver
            .ipv4_lookup("tcp-fallback.test.")
            .await
            .expect("retry truncated response over TCP");
        assert_eq!(answer.answers().len(), 1);
        assert_eq!(answer.answers()[0].data.to_string(), "127.0.0.1");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn lease_close_stops_real_dns_retries_after_late_failure() {
    const INITIAL_QUERIES: usize = 2;

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind late-response TCP DNS server");
    listener
        .set_nonblocking(true)
        .expect("configure late-response TCP DNS server");
    let dns_address = listener.local_addr().expect("late-response DNS address");
    let socket = std::net::UdpSocket::bind(dns_address).expect("bind late-response UDP DNS server");
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set initial DNS timeout");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut packet = [0_u8; 2_048];
        let mut requests = Vec::with_capacity(INITIAL_QUERIES);
        for _ in 0..INITIAL_QUERIES {
            let (length, peer) = socket.recv_from(&mut packet).expect("receive DNS query");
            requests.push((packet[..length].to_vec(), peer));
        }
        ready_tx.send(()).expect("report initial DNS queries");
        release_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release late DNS responses");
        for (query, peer) in &requests {
            socket
                .send_to(&local_servfail_response(query), peer)
                .expect("send late DNS failure");
        }

        socket
            .set_nonblocking(true)
            .expect("configure retry observation");
        let mut retries = 0;
        let observation_deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < observation_deadline {
            match socket.recv_from(&mut packet) {
                Ok(_) => retries += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("observe DNS retries: {error}"),
            }
            match listener.accept() {
                Ok(_) => retries += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("observe TCP DNS retries: {error}"),
            }
            thread::sleep(Duration::from_millis(5));
        }
        retries
    });

    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_dns_server(dns_address)
            .with_dns_cache(0, Duration::ZERO),
    )
    .expect("start explicit DNS proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("cancel-wire.test", 443),
        )
        .expect("attach DNS lease");
    let mut client = std::net::TcpStream::connect(lease.endpoint().socket_addr())
        .expect("connect explicit DNS proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT cancel-wire.test:443 HTTP/1.1\r\nHost: cancel-wire.test\r\n\r\n",
    )
    .expect("write DNS-bound CONNECT");
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("observe initial wire queries");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close DNS-bound lease");
    let final_usage = final_usage.usage();
    assert_eq!(final_usage.accepted_connections, 1);
    assert_eq!(final_usage.active_connections, 0);
    release_tx.send(()).expect("release late DNS failures");
    assert_client_stopped(client);
    assert_eq!(
        server.join().expect("join late-response DNS server"),
        0,
        "cancelled lookup must not retry after a late DNS failure"
    );
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("shutdown explicit DNS proxy");
}

#[test]
fn zero_capacity_resolver_cache_requeries_local_dns() {
    let (address, server) = start_local_dns(2, local_a_response);
    let config = ProxyConfig::default().with_dns_cache(0, Duration::from_secs(60));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        for _ in 0..2 {
            let answer = resolver
                .lookup_ip("cache-disabled.test.")
                .await
                .expect("resolve through local DNS");
            assert_eq!(
                answer.iter().collect::<Vec<_>>(),
                vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]
            );
        }
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_cache_ttl_ceiling_expires_local_dns_answer() {
    let (address, server) = start_local_dns(2, local_a_response);
    let config = ProxyConfig::default().with_dns_cache(8, Duration::from_secs(1));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        let first = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve initial local answer");
        let cached = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve cached local answer");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            cached.iter().collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let expired = resolver
            .lookup_ip("cache-expiry.test.")
            .await
            .expect("resolve expired local answer again");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            expired.iter().collect::<Vec<_>>()
        );
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_cache_ttl_ceiling_expires_local_nxdomain() {
    let (address, server) = start_local_dns(2, local_nxdomain_response);
    let config = ProxyConfig::default().with_dns_cache(8, Duration::from_secs(1));
    let resolver = local_dns_resolver(address, &config);
    let runtime = tokio::runtime::Runtime::new().expect("start resolver test runtime");

    runtime.block_on(async {
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("initial NXDOMAIN must fail");
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("cached NXDOMAIN must fail");
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        resolver
            .lookup_ip("negative-cache.test.")
            .await
            .expect_err("expired NXDOMAIN must be queried again");
    });
    server.join().expect("join local DNS server");
}

#[test]
fn resolver_answers_are_rechecked_after_identity_reuse() {
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO),
        resolver,
        connector,
    )
    .expect("start identity-reuse proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let first_policy = Policy::builder()
        .allow_host("reused.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback network"))
        .allow_port(443)
        .build()
        .expect("valid first policy");
    let first = proxy
        .attach(identity.clone(), first_policy)
        .expect("attach first lease");
    let endpoint = first.endpoint().socket_addr();
    let mut first_client = std::net::TcpStream::connect(endpoint).expect("connect first lease");
    std::io::Write::write_all(
        &mut first_client,
        b"CONNECT reused.test:443 HTTP/1.1\r\nHost: reused.test\r\n\r\n",
    )
    .expect("write first CONNECT");
    let mut first_response = String::new();
    std::io::Read::read_to_string(&mut first_client, &mut first_response)
        .expect("read first denial");
    assert!(first_response.contains("dial-failed"), "{first_response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    first
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close first lease");

    let second_policy = Policy::builder()
        .allow_host("reused.test")
        .expect("valid hostname")
        .allow_port(443)
        .build()
        .expect("valid second policy");
    let second = proxy
        .attach(identity, second_policy)
        .expect("attach second lease");
    let mut second_client = std::net::TcpStream::connect(endpoint).expect("connect second lease");
    std::io::Write::write_all(
        &mut second_client,
        b"CONNECT reused.test:443 HTTP/1.1\r\nHost: reused.test\r\n\r\n",
    )
    .expect("write second CONNECT");
    let mut second_response = String::new();
    std::io::Read::read_to_string(&mut second_client, &mut second_response)
        .expect("read second denial");
    assert!(
        second_response.contains("resolved-address-denied"),
        "{second_response}"
    );
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);
    second
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close second lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn explicit_public_network_denial_blocks_dns_and_literal_paths_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("blocked-public.test")
        .expect("valid test hostname")
        .allow_network("0.0.0.0/0".parse().expect("valid catch-all grant"))
        .deny_network(
            "93.184.216.0/24"
                .parse()
                .expect("valid public denial network"),
        )
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");

    for (authority, reason) in [
        ("blocked-public.test", "resolved-address-denied"),
        ("93.184.216.34", "ip-literal-denied"),
    ] {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            format!("CONNECT {authority}:443 HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains(reason), "{response}");
    }

    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 2);
    assert_eq!(usage.denied_connections, 2);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn azure_wireserver_dns_answer_is_rejected_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "168.63.129.16".parse().expect("Azure WireServer address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("wireserver.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT wireserver.test:443 HTTP/1.1\r\nHost: wireserver.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn hostname_resolving_to_proxy_listener_is_rejected_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    )]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let endpoint = proxy.endpoint().socket_addr();
    let policy = Policy::builder()
        .allow_host("self.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("loopback grant"))
        .allow_port(endpoint.port())
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client = std::net::TcpStream::connect(endpoint).expect("connect proxy listener");
    std::io::Write::write_all(
        &mut client,
        format!(
            "CONNECT self.test:{} HTTP/1.1\r\nHost: self.test\r\n\r\n",
            endpoint.port()
        )
        .as_bytes(),
    )
    .expect("write self-directed hostname CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read proxy endpoint denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("proxy-endpoint-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn wildcard_listener_rejects_every_same_port_destination_before_dial() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default().with_bind_address("[::]:0".parse().expect("wildcard bind")),
        resolver,
        connector,
    )
    .expect("start wildcard proxy");
    let port = proxy.endpoint().socket_addr().port();
    let policy = Policy::builder()
        .allow_host("same-port.test")
        .expect("valid hostname")
        .allow_port(port)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .expect("connect wildcard listener");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT same-port.test:{port} HTTP/1.1\r\nHost: same-port.test\r\n\r\n")
            .as_bytes(),
    )
    .expect("write same-port CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response)
        .expect("read wildcard endpoint denial");

    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("proxy-endpoint-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn proxy_endpoint_matching_covers_transport_spellings_and_wildcard_port() {
    let endpoint: SocketAddr = "127.0.0.1:4750".parse().expect("IPv4 endpoint");
    assert!(is_proxy_endpoint(endpoint, endpoint));
    assert!(is_proxy_endpoint(
        "[::ffff:127.0.0.1]:4750".parse().expect("mapped endpoint"),
        endpoint,
    ));
    assert!(is_proxy_endpoint(
        "127.0.0.1:4750".parse().expect("IPv4 loopback"),
        "[::]:4750".parse().expect("dual-stack wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "127.0.0.1:4750".parse().expect("IPv4 loopback"),
        "0.0.0.0:4750".parse().expect("IPv4 wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "93.184.216.34:4750".parse().expect("remote endpoint"),
        "[::]:4750".parse().expect("dual-stack wildcard"),
    ));
    assert!(is_proxy_endpoint(
        "10.0.0.8:4750"
            .parse()
            .expect("private interface candidate"),
        "0.0.0.0:4750".parse().expect("IPv4 wildcard"),
    ));
    assert!(!is_proxy_endpoint(
        "127.0.0.1:4751".parse().expect("different port"),
        endpoint,
    ));
}

#[test]
fn failed_startup_joins_the_owned_runtime_thread() {
    let reservation =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("reserve listener");
    let address = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let result = Proxy::start_with_test_resolver(
        ProxyConfig::default()
            .with_bind_address(address)
            .with_upstream_proxy(address),
        Arc::new(StartupDropProbe(Arc::clone(&dropped))),
    );

    assert!(result.is_err(), "self-referencing startup succeeded");
    assert!(
        dropped.load(Ordering::Acquire),
        "startup returned before its runtime-owned resolver was dropped"
    );
}

#[test]
fn explicit_hostname_denial_stops_before_dns_and_dial() {
    let (resolved_tx, resolved_rx) = mpsc::channel();
    let resolver = Arc::new(CapturingResolver(resolved_tx));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("*.example.test")
        .expect("valid wildcard grant")
        .deny_host("admin.example.test")
        .expect("valid hostname denial")
        .deny_host("*.internal.example.test")
        .expect("valid wildcard denial")
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    for authority in [
        "AdMiN.ExAmPlE.TeSt.:443",
        "deep.secret.internal.example.test:443",
    ] {
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        std::io::Write::write_all(&mut client, request.as_bytes()).expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read hostname denial");

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("host-denied"), "{response}");
    }
    assert!(matches!(
        resolved_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(usage.accepted_connections, 2);
    assert_eq!(usage.denied_connections, 2);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dns_answer_with_ipv4_compatible_metadata_address_is_rejected_as_a_set() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "93.184.216.34".parse().expect("public test address"),
        "::169.254.169.254"
            .parse()
            .expect("compatible metadata address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("mixed.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT mixed.test:443 HTTP/1.1\r\nHost: mixed.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn legacy_numeric_spellings_cannot_bypass_the_system_resolver_address_floor() {
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for hostname in ["127.1", "0177.0.0.1", "0x7f000001", "2130706433"] {
        assert!(hostname.parse::<IpAddr>().is_err(), "{hostname}");
        let (dns_address, dns_server) = start_local_dns(2, local_a_response);
        let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
        let proxy = Proxy::start_with_test_connector(
            ProxyConfig::default().with_dns_server(dns_address),
            connector,
        )
        .expect("start explicit-DNS proxy");
        let lease = proxy
            .attach(
                PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                Policy::builder()
                    .allow_host(hostname)
                    .expect("valid legacy-looking hostname")
                    .allow_port(443)
                    .build()
                    .expect("valid policy"),
            )
            .expect("attach lease");
        let mut client =
            std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        std::io::Write::write_all(
            &mut client,
            format!("CONNECT {hostname}:443 HTTP/1.1\r\nHost: {hostname}\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut client, &mut response).expect("read address denial");
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "{hostname}: {response}"
        );
        assert!(
            response.contains("resolved-address-denied"),
            "{hostname}: {response}"
        );
        assert_eq!(dial_attempts.load(Ordering::Acquire), 0, "{hostname}");

        lease
            .close(Instant::now() + Duration::from_secs(1))
            .expect("close lease");
        proxy
            .shutdown(Instant::now() + Duration::from_secs(1))
            .expect("proxy shutdown");
        dns_server.join().expect("join explicit DNS server");
    }
}

#[test]
fn configured_nat64_prefix_rejects_an_embedded_metadata_destination() {
    let resolver = Arc::new(FixedAnswerResolver(vec![
        "2600:1f18:abcd:1234::a9fe:a9fe"
            .parse()
            .expect("network-specific NAT64 metadata address"),
    ]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let config = ProxyConfig::default().with_nat64_prefix(
        "2600:1f18:abcd:1234::/96"
            .parse()
            .expect("network-specific NAT64 prefix"),
    );
    let proxy = Proxy::start_with_test_backends(config, resolver, connector)
        .expect("start NAT64-aware proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("nat64.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT nat64.test:443 HTTP/1.1\r\nHost: nat64.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read NAT64 denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("resolved-address-denied"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn oversized_dns_answer_is_rejected_before_any_dial() {
    let loopback = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let resolver = Arc::new(FixedAnswerResolver(vec![loopback; 65]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("large-answer.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT large-answer.test:443 HTTP/1.1\r\nHost: large-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read DNS denial");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains("dns-answer-too-large"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 0);
    assert_eq!(lease.usage().denied_connections, 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn duplicate_dns_answers_produce_one_dial_attempt() {
    let loopback = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    let resolver = Arc::new(FixedAnswerResolver(vec![loopback; 64]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            hostname_policy("duplicate-answer.test", 443),
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT duplicate-answer.test:443 HTTP/1.1\r\nHost: duplicate-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn equivalent_dns_address_spellings_produce_one_dial_attempt() {
    let ipv4 = std::net::Ipv4Addr::LOCALHOST;
    let mapped = IpAddr::V6(ipv4.to_ipv6_mapped());
    let resolver = Arc::new(FixedAnswerResolver(vec![IpAddr::V4(ipv4), mapped]));
    let dial_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector = Arc::new(RejectingConnector(Arc::clone(&dial_attempts)));
    let proxy = Proxy::start_with_test_backends(ProxyConfig::default(), resolver, connector)
        .expect("start proxy");
    let policy = Policy::builder()
        .allow_host("equivalent-answer.test")
        .expect("valid hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid IPv4 loopback grant"))
        .allow_network(
            "::ffff:127.0.0.1/128"
                .parse()
                .expect("valid mapped loopback grant"),
        )
        .allow_port(443)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach lease");
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    std::io::Write::write_all(
        &mut client,
        b"CONNECT equivalent-answer.test:443 HTTP/1.1\r\nHost: equivalent-answer.test\r\n\r\n",
    )
    .expect("write CONNECT");
    let mut response = String::new();
    std::io::Read::read_to_string(&mut client, &mut response).expect("read dial denial");
    assert!(response.contains("dial-failed"), "{response}");
    assert_eq!(dial_attempts.load(Ordering::Acquire), 1);

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
