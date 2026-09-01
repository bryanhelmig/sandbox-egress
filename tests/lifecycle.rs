//! End-to-end lease lifecycle and tunnelling tests.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use sandbox_egress::{AttachError, CloseErrorKind, PeerIdentity, Policy, Proxy, ProxyConfig};

fn local_policy(port: u16) -> Policy {
    Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(port)
        .max_connections(16)
        .expect("positive limit")
        .build()
        .expect("valid policy")
}

fn start_echo() -> (u16, thread::JoinHandle<()>) {
    start_echo_on(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn start_echo_on(address: IpAddr) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(SocketAddr::new(address, 0)).expect("bind echo");
    let port = listener.local_addr().expect("echo address").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept echo");
        let mut buffer = [0_u8; 128];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                break;
            }
            if stream.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
    });
    (port, handle)
}

fn start_capture() -> (u16, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind capture");
    listener.set_nonblocking(true).expect("nonblocking capture");
    let port = listener.local_addr().expect("capture address").port();
    let (sender, receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(250);
        let captured = loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking capture");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .expect("capture timeout");
                    let mut buffer = [0_u8; 128];
                    let bytes = stream.read(&mut buffer).unwrap_or(0);
                    break buffer[..bytes].to_vec();
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::yield_now();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break Vec::new(),
                Err(error) => panic!("capture accept failed: {error}"),
            }
        };
        sender.send(captured).expect("send captured bytes");
    });
    (port, receiver, handle)
}

fn attach_local(proxy: &Proxy, policy: Policy) -> sandbox_egress::Lease {
    proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach localhost")
}

fn read_blocking_header(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read complete header");
        request.push(byte[0]);
    }
    request
}

fn header_denial(config: ProxyConfig, bytes: &[u8], finish_upload: bool) -> String {
    let proxy = Proxy::start(config).expect("start proxy");
    let lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set denial timeout");
    client.write_all(bytes).expect("write header bytes");
    if finish_upload {
        client
            .shutdown(Shutdown::Write)
            .expect("finish header upload");
    }
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read header denial");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close denied lease")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
    response
}

#[test]
fn connect_tunnels_and_accounts_bytes() {
    let (port, echo) = start_echo();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(&proxy, local_policy(port));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    client.write_all(b"ping").expect("write tunnel");
    let mut echoed = [0_u8; 4];
    client.read_exact(&mut echoed).expect("read tunnel");
    assert_eq!(&echoed, b"ping");
    client
        .shutdown(Shutdown::Write)
        .expect("finish tunnel upload");
    let mut trailing = Vec::new();
    client
        .read_to_end(&mut trailing)
        .expect("read tunnel shutdown");
    drop(client);
    echo.join().expect("echo thread");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("certified close")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.accepted_connections, 1);
    assert_eq!(final_usage.completed_connections, 1);
    assert_eq!(final_usage.uploaded_bytes, 4);
    assert_eq!(final_usage.downloaded_bytes, 4);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
}

#[test]
fn upstream_proxy_receives_only_the_approved_numeric_destination() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream proxy");
    let upstream_proxy = listener.local_addr().expect("upstream proxy address");
    let (request_tx, request_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream proxy connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set upstream proxy timeout");
        let request = read_blocking_header(&mut stream);
        request_tx.send(request).expect("capture upstream CONNECT");
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nhello")
            .expect("approve upstream CONNECT with coalesced payload");
        let mut upload = [0_u8; 4];
        stream
            .read_exact(&mut upload)
            .expect("read tunneled upload");
        assert_eq!(&upload, b"ping");
        stream.write_all(b"pong").expect("write tunneled download");
        let mut trailing = Vec::new();
        stream
            .read_to_end(&mut trailing)
            .expect("read tunneled upload shutdown");
    });

    let target: SocketAddr = "127.0.0.2:443".parse().expect("numeric target");
    let proxy = Proxy::start(ProxyConfig::default().with_upstream_proxy(upstream_proxy))
        .expect("start proxy");
    let lease = attach_local(&proxy, local_policy(target.port()));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            format!(
                "CONNECT {target} HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: Basic guest-controlled\r\nX-Guest-Run: forged\r\n\r\n",
                target.ip()
            )
            .as_bytes(),
        )
        .expect("write guest CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read guest CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let mut greeting = [0_u8; 5];
    client
        .read_exact(&mut greeting)
        .expect("read coalesced upstream payload");
    assert_eq!(&greeting, b"hello");
    client.write_all(b"ping").expect("write tunneled upload");
    let mut pong = [0_u8; 4];
    client
        .read_exact(&mut pong)
        .expect("read tunneled download");
    assert_eq!(&pong, b"pong");
    client
        .shutdown(Shutdown::Write)
        .expect("finish tunneled upload");
    let mut trailing = Vec::new();
    client
        .read_to_end(&mut trailing)
        .expect("read tunnel shutdown");
    server.join().expect("join upstream proxy");

    assert_eq!(
        request_rx.recv().expect("receive upstream CONNECT"),
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").into_bytes()
    );
    let usage = lease
        .close(Instant::now() + Duration::from_secs(2))
        .expect("certified close")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.completed_connections, 1);
    assert_eq!(usage.uploaded_bytes, 4);
    assert_eq!(usage.downloaded_bytes, 9);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(2))
        .expect("proxy shutdown");
}

#[test]
fn upstream_proxy_refusal_has_a_distinct_bounded_denial() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream proxy");
    let upstream_proxy = listener.local_addr().expect("upstream proxy address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream proxy connection");
        read_blocking_header(&mut stream);
        stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .expect("refuse upstream CONNECT");
    });

    let target: SocketAddr = "127.0.0.2:443".parse().expect("numeric target");
    let proxy = Proxy::start(ProxyConfig::default().with_upstream_proxy(upstream_proxy))
        .expect("start proxy");
    let lease = attach_local(&proxy, local_policy(target.port()));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {}\r\n\r\n", target.ip()).as_bytes())
        .expect("write guest CONNECT");
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read upstream refusal denial");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains("upstream-proxy-failed"), "{response}");
    server.join().expect("join upstream proxy");

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("certified close")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upstream_proxy_response_header_is_bounded() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream proxy");
    let upstream_proxy = listener.local_addr().expect("upstream proxy address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream proxy connection");
        read_blocking_header(&mut stream);
        let response = vec![b'x'; 32 * 1_024].into_boxed_slice();
        stream
            .write_all(&response)
            .expect("write bounded invalid response");
    });

    let target: SocketAddr = "127.0.0.2:443".parse().expect("numeric target");
    let proxy = Proxy::start(ProxyConfig::default().with_upstream_proxy(upstream_proxy))
        .expect("start proxy");
    let lease = attach_local(&proxy, local_policy(target.port()));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {}\r\n\r\n", target.ip()).as_bytes())
        .expect("write guest CONNECT");
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read bounded upstream failure");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(response.contains("upstream-proxy-failed"), "{response}");
    server.join().expect("join upstream proxy");

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("certified close")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn close_cancels_a_pending_upstream_proxy_handshake() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream proxy");
    let upstream_proxy = listener.local_addr().expect("upstream proxy address");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let (terminal_tx, terminal_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream proxy connection");
        read_blocking_header(&mut stream);
        entered_tx.send(()).expect("signal pending handshake");
        release_rx.recv().expect("release upstream observer");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set terminal timeout");
        let mut byte = [0_u8; 1];
        let terminal = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            _ => false,
        };
        terminal_tx.send(terminal).expect("send terminal state");
    });

    let target: SocketAddr = "127.0.0.2:443".parse().expect("numeric target");
    let proxy = Proxy::start(ProxyConfig::default().with_upstream_proxy(upstream_proxy))
        .expect("start proxy");
    let lease = attach_local(&proxy, local_policy(target.port()));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set guest terminal timeout");
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {}\r\n\r\n", target.ip()).as_bytes())
        .expect("write guest CONNECT");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("observe pending upstream handshake");

    let started = Instant::now();
    let usage = lease
        .close(Instant::now() + Duration::from_millis(500))
        .expect("cancel pending upstream handshake")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 0);
    assert_eq!(usage.active_connections, 0);
    let mut byte = [0_u8; 1];
    assert!(
        !matches!(
            client.read(&mut byte),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
        ),
        "guest socket remained open"
    );
    release_tx.send(()).expect("release upstream observer");
    assert!(
        terminal_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("observe upstream terminal state")
    );
    server.join().expect("join upstream proxy");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upstream_proxy_cannot_reference_the_shared_listener() {
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve listener port");
    let address = reservation.local_addr().expect("reserved listener address");
    drop(reservation);

    let result = Proxy::start(
        ProxyConfig::default()
            .with_bind_address(address)
            .with_upstream_proxy(address),
    );
    assert!(result.is_err(), "recursive upstream proxy was accepted");
}

#[test]
fn lease_close_observes_a_completed_proxy_shutdown() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(b"CONNECT denied.test:443 HTTP/1.1\r\nHost: denied.test\r\n\r\n")
        .expect("write denied CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    assert!(response.contains("port-denied"), "{response}");

    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("certified proxy shutdown");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("observe proxy-wide certificate")
        .usage();
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.accepted_connections, 1);
    assert_eq!(final_usage.denied_connections, 1);
}

#[test]
fn close_revokes_a_slow_header_without_waiting_for_the_peer() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(
        &proxy,
        Policy::builder()
            .allow_host("example.com")
            .expect("valid host")
            .build()
            .expect("valid policy"),
    );
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client.write_all(b"CON").expect("partial header");
    thread::sleep(Duration::from_millis(20));

    let started = Instant::now();
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close slow header")
        .usage();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(final_usage.active_connections, 0);

    client
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout");
    let mut byte = [0_u8; 1];
    assert!(matches!(client.read(&mut byte), Ok(0) | Err(_)));
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn oversized_header_has_a_distinct_denial() {
    let response = header_denial(
        ProxyConfig::default().with_max_header_bytes(1_024),
        &[b'x'; 1_024],
        false,
    );
    assert!(response.starts_with("HTTP/1.1 431"), "{response}");
    assert!(response.contains("header-too-large"), "{response}");
}

#[test]
fn excess_header_count_has_a_distinct_denial() {
    let (diagnostic_tx, diagnostic_rx) = mpsc::sync_channel(1);
    let mut request = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n".to_vec();
    for index in 0..64 {
        request.extend_from_slice(format!("attacker-{index}: secret-{index}\r\n").as_bytes());
    }
    request.extend_from_slice(b"\r\n");

    let response = header_denial(
        ProxyConfig::default().with_diagnostic_channel(diagnostic_tx, 1),
        &request,
        false,
    );
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("too-many-headers"), "{response}");
    let diagnostic = diagnostic_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("header-count diagnostic");
    assert_eq!(diagnostic.reason.as_str(), "too-many-headers");
    assert!(!format!("{diagnostic:?}").contains("secret"));
}

#[test]
fn bracketed_non_ipv6_host_has_a_distinct_denial() {
    let response = header_denial(
        ProxyConfig::default(),
        b"CONNECT [example.com]:443 HTTP/1.1\r\n\r\n",
        false,
    );
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("invalid-ipv6-literal"), "{response}");
}

#[test]
fn mismatched_host_header_is_denied_before_policy_and_dns() {
    let response = header_denial(
        ProxyConfig::default(),
        b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: forbidden.test\r\n\r\n",
        false,
    );
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("host-header-mismatch"), "{response}");
}

#[test]
fn early_header_eof_has_a_distinct_denial() {
    let response = header_denial(ProxyConfig::default(), b"CONNECT incomplete", true);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(response.contains("header-eof"), "{response}");
}

#[test]
fn header_deadline_has_a_distinct_denial() {
    let response = header_denial(
        ProxyConfig::default().with_header_timeout(Duration::from_millis(20)),
        b"CONNECT slow",
        false,
    );
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    assert!(response.contains("header-timeout"), "{response}");
}

#[test]
fn arrivals_that_prevent_quiet_close_return_the_owning_lease() {
    let config =
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::from_millis(200));
    let proxy = Proxy::start(config).expect("start proxy");
    let lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));
    let endpoint = lease.endpoint().socket_addr();
    let old_arrival = thread::spawn(move || {
        thread::sleep(Duration::from_millis(75));
        let mut client = TcpStream::connect(endpoint).expect("connect old queued socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("old socket read timeout");
        let mut byte = [0];
        assert!(matches!(client.read(&mut byte), Ok(0)));
    });

    let error = lease
        .close(Instant::now() + Duration::from_millis(225))
        .expect_err("continued old arrivals must prevent quiet-period close");
    old_arrival.join().expect("old queued socket");
    assert_eq!(error.kind(), CloseErrorKind::DeadlineExceeded);
    let lease = error.into_lease();
    assert_eq!(lease.usage().denied_connections, 1);
    let replacement = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder().build().expect("valid policy"),
        )
        .expect_err("failed close must retain identity ownership");
    assert!(
        matches!(replacement, AttachError::IdentityInUse),
        "unexpected replacement result: {replacement}"
    );

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("retry close");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn repeated_failed_close_retries_preserve_identity_and_counters() {
    let config =
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::from_millis(300));
    let proxy = Proxy::start(config).expect("start proxy");
    let mut lease = attach_local(&proxy, Policy::builder().build().expect("valid policy"));
    let lease_id = lease.id();
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(b"CONNECT denied.test:443 HTTP/1.1\r\nHost: denied.test\r\n\r\n")
        .expect("write denied CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    assert!(response.contains("port-denied"), "{response}");
    let accounting_deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().active_connections != 0 && Instant::now() < accounting_deadline {
        thread::yield_now();
    }
    let expected = lease.usage();
    assert_eq!(expected.accepted_connections, 1);
    assert_eq!(expected.denied_connections, 1);
    assert_eq!(expected.active_connections, 0);

    for attempt in 1..=3 {
        let error = lease
            .close(Instant::now() + Duration::from_millis(10))
            .expect_err("short close must retain the lease");
        assert_eq!(error.kind(), CloseErrorKind::DeadlineExceeded);
        lease = error.into_lease();
        assert_eq!(lease.id(), lease_id, "attempt {attempt} changed ownership");
        assert_eq!(
            lease.usage(),
            expected,
            "attempt {attempt} changed counters"
        );
        assert!(matches!(
            proxy.attach(
                PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                Policy::builder().build().expect("replacement policy"),
            ),
            Err(AttachError::IdentityInUse)
        ));
    }

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("retry must eventually certify close")
        .usage();
    assert_eq!(final_usage, expected);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn hostname_policy_denies_before_dns() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let lease = attach_local(
        &proxy,
        Policy::builder()
            .allow_port(443)
            .build()
            .expect("valid policy"),
    );
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(b"CONNECT forbidden.invalid:443 HTTP/1.1\r\nHost: forbidden.invalid\r\n\r\n")
        .expect("write CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("host-denied"), "{response}");
    assert_eq!(lease.usage().denied_connections, 1);
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn proxy_listener_cannot_be_a_tunnel_destination() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let endpoint = proxy.endpoint().socket_addr();
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(endpoint.port())
        .build()
        .expect("valid policy");
    let lease = attach_local(&proxy, policy);
    let mut client = TcpStream::connect(endpoint).expect("connect proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set response timeout");
    client
        .write_all(
            format!(
                "CONNECT {endpoint} HTTP/1.1\r\nHost: {}\r\n\r\n",
                endpoint.ip()
            )
            .as_bytes(),
        )
        .expect("write self-directed CONNECT");
    let mut response = [0_u8; 256];
    let bytes = client
        .read(&mut response)
        .expect("read self-connection denial");
    assert!(response[..bytes].starts_with(b"HTTP/1.1 403"));
    assert!(String::from_utf8_lossy(&response[..bytes]).contains("proxy-endpoint-denied"));

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close self-connection lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    assert_eq!(usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn an_explicit_http_port_does_not_also_allow_https() {
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(80)
        .build()
        .expect("valid HTTP-only policy");
    let lease = attach_local(&proxy, policy);
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .expect("write disallowed HTTPS CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("port-denied"), "{response}");

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close HTTP-only lease")
        .usage();
    assert_eq!(usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn diagnostics_retain_lease_attribution_across_identity_reuse() {
    let (diagnostic_tx, diagnostic_rx) = mpsc::sync_channel(4);
    let proxy = Proxy::start(ProxyConfig::default().with_diagnostic_channel(diagnostic_tx, 10))
        .expect("start proxy");
    let identity = PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let lease = proxy
        .attach(
            identity.clone(),
            Policy::builder()
                .allow_port(443)
                .build()
                .expect("valid policy"),
        )
        .expect("attach lease");
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            b"CONNECT attacker-controlled.invalid:443 HTTP/1.1\r\nHost: attacker-controlled.invalid\r\n\r\n",
        )
        .expect("write CONNECT");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read denial");
    let old_lease_id = lease.id();
    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close first lease");

    let replacement = proxy
        .attach(
            identity.clone(),
            Policy::builder()
                .allow_port(443)
                .build()
                .expect("valid policy"),
        )
        .expect("reuse identity after certified close");
    let mut client = TcpStream::connect(replacement.endpoint().socket_addr())
        .expect("connect replacement lease");
    client
        .write_all(
            b"CONNECT another-attacker-value.invalid:443 HTTP/1.1\r\nHost: another-attacker-value.invalid\r\n\r\n",
        )
        .expect("write replacement CONNECT");
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read replacement denial");

    let old_event = diagnostic_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("old denial diagnostic");
    let replacement_event = diagnostic_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("replacement denial diagnostic");
    assert_eq!(old_event.lease_id, old_lease_id);
    assert_eq!(old_event.identity, identity);
    assert_eq!(old_event.reason.as_str(), "host-denied");
    assert_eq!(old_event.suppressed_before, 0);
    assert_ne!(replacement.id(), old_lease_id);
    assert_eq!(replacement_event.lease_id, replacement.id());
    assert_eq!(replacement_event.identity, identity);
    assert_eq!(replacement_event.reason.as_str(), "host-denied");
    assert_eq!(replacement_event.suppressed_before, 0);

    replacement
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close replacement lease");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn bracketed_ipv6_literal_is_checked_and_dialed_directly() {
    let (port, echo) = start_echo_on(IpAddr::V6(Ipv6Addr::LOCALHOST));
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_bind_address(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)),
    )
    .expect("start IPv6 proxy");
    let policy = Policy::builder()
        .allow_network("::1/128".parse::<IpNet>().expect("IPv6 test CIDR"))
        .allow_port(port)
        .build()
        .expect("valid policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            policy,
        )
        .expect("attach IPv6 identity");
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(format!("CONNECT [::1]:{port} HTTP/1.1\r\nHost: [::1]\r\n\r\n").as_bytes())
        .expect("write IPv6 CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");

    lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close IPv6 lease");
    drop(client);
    echo.join().expect("echo thread");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn dual_stack_listener_maps_ipv4_peer_to_the_ipv4_lease() {
    let proxy = Proxy::start(
        ProxyConfig::default()
            .with_bind_address(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)),
    )
    .expect("start dual-stack proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Policy::builder()
                .allow_port(443)
                .build()
                .expect("valid policy"),
        )
        .expect("attach IPv4 lease");
    let mut client = TcpStream::connect(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        lease.endpoint().socket_addr().port(),
    ))
    .expect("connect IPv4 client to dual-stack listener");
    client
        .write_all(b"CONNECT denied.test:443 HTTP/1.1\r\nHost: denied.test\r\n\r\n")
        .expect("write denied CONNECT");
    let mut response = String::new();
    client
        .read_to_string(&mut response)
        .expect("read owned denial");
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("host-denied"), "{response}");

    let usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close IPv4 lease")
        .usage();
    assert_eq!(usage.accepted_connections, 1);
    assert_eq!(usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upload_limit_blocks_payload_coalesced_with_connect_header() {
    let (port, captured, capture_thread) = start_capture();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(port)
        .max_upload_bytes(0)
        .build()
        .expect("valid policy");
    let lease = attach_local(&proxy, policy);
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nsecret")
                .as_bytes(),
        )
        .expect("write coalesced payload");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("read response");

    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert_eq!(
        captured
            .recv_timeout(Duration::from_secs(1))
            .expect("captured upstream bytes"),
        b""
    );
    capture_thread.join().expect("capture thread");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close upload-limited lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, 6);
    assert_eq!(final_usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upload_limit_allows_exact_coalesced_boundary() {
    let (port, captured, capture_thread) = start_capture();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(port)
        .max_upload_bytes(6)
        .build()
        .expect("valid policy");
    let lease = attach_local(&proxy, policy);
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nsecret")
                .as_bytes(),
        )
        .expect("write coalesced payload");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    assert_eq!(
        captured
            .recv_timeout(Duration::from_secs(1))
            .expect("captured upstream bytes"),
        b"secret"
    );
    capture_thread.join().expect("capture thread");
    client
        .read_to_end(&mut Vec::new())
        .expect("read tunnel closure");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close upload-limited lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, 6);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upload_limit_blocks_payload_sent_after_connect_response() {
    let (port, captured, capture_thread) = start_capture();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(port)
        .max_upload_bytes(0)
        .build()
        .expect("valid policy");
    let lease = attach_local(&proxy, policy);
    let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .write_all(
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
        )
        .expect("write CONNECT");
    let mut response = [0_u8; 39];
    client
        .read_exact(&mut response)
        .expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    client.write_all(b"secret").expect("write limited payload");
    let mut closed = Vec::new();
    client
        .read_to_end(&mut closed)
        .expect("read tunnel closure");

    assert_eq!(
        captured
            .recv_timeout(Duration::from_secs(1))
            .expect("captured upstream bytes"),
        b""
    );
    capture_thread.join().expect("capture thread");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close upload-limited lease")
        .usage();
    assert!(final_usage.uploaded_bytes <= 6);
    assert_eq!(final_usage.denied_connections, 1);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn upload_limit_is_independent_for_each_tunnel() {
    let (first_port, first_echo) = start_echo();
    let (second_port, second_echo) = start_echo();
    let proxy = Proxy::start(ProxyConfig::default()).expect("start proxy");
    let policy = Policy::builder()
        .allow_network("127.0.0.0/8".parse::<IpNet>().expect("test CIDR"))
        .allow_port(first_port)
        .allow_port(second_port)
        .max_upload_bytes(1)
        .build()
        .expect("valid policy");
    let lease = attach_local(&proxy, policy);

    for port in [first_port, second_port] {
        let mut client = TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
            )
            .expect("write CONNECT");
        let mut response = [0_u8; 39];
        client
            .read_exact(&mut response)
            .expect("read CONNECT response");
        assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        client.write_all(b"x").expect("write one allowed byte");
        let mut echoed = [0_u8; 1];
        client.read_exact(&mut echoed).expect("read echoed byte");
        assert_eq!(&echoed, b"x");
    }

    first_echo.join().expect("first echo thread");
    second_echo.join().expect("second echo thread");
    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, 2);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
