use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::proxy::{TestConnector, TestResolver};
use crate::tls::fixtures::{client_hello, client_hello_with_padding, fragment_records};
use crate::{EchPolicy, Lease, PeerIdentity, Policy, Proxy, ProxyConfig, TlsAuthority};

struct StaticResolver(IpAddr);

struct ConstrainedConnector {
    send_buffer_bytes: usize,
}

impl TestConnector for ConstrainedConnector {
    fn connect(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send + '_>> {
        Box::pin(async move {
            let stream = TcpStream::connect(address).await?;
            socket2::SockRef::from(&stream).set_send_buffer_size(self.send_buffer_bytes)?;
            Ok(stream)
        })
    }
}

impl TestResolver for StaticResolver {
    fn lookup<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move { Ok(vec![self.0]) })
    }
}

fn tls_hostname_policy(hostname: &str, port: u16, ech: EchPolicy) -> Policy {
    Policy::builder()
        .allow_host(hostname)
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(port)
        .tls_authority(TlsAuthority::RequireVisibleSni { ech })
        .dns_timeout(Duration::from_secs(2))
        .handshake_timeout(Duration::from_secs(2))
        .build()
        .expect("valid policy")
}

fn start_tls_capture(expected: usize) -> (u16, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind TLS capture");
    let port = listener.local_addr().expect("TLS capture address").port();
    let (captured_tx, captured_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept inspected dial");
        let mut captured = vec![0; expected];
        std::io::Read::read_exact(&mut stream, &mut captured).expect("read inspected hello");
        captured_tx.send(captured).expect("send captured hello");
        std::io::Write::write_all(&mut stream, b"x").expect("write capture marker");
    });
    (port, captured_rx, handle)
}

fn start_tls_denial_observer() -> (u16, mpsc::Receiver<usize>, thread::JoinHandle<()>) {
    let listener =
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind TLS denial observer");
    let port = listener.local_addr().expect("observer address").port();
    let (observed_tx, observed_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept inspected dial");
        let mut observed = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut observed).expect("observe denied tunnel");
        observed_tx
            .send(observed.len())
            .expect("send observed size");
    });
    (port, observed_rx, handle)
}

fn start_nonreading_target() -> (
    u16,
    mpsc::Sender<()>,
    mpsc::Receiver<usize>,
    thread::JoinHandle<()>,
) {
    let listener =
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind nonreading target");
    let port = listener.local_addr().expect("nonreading address").port();
    let (release_tx, release_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept constrained dial");
        socket2::SockRef::from(&stream)
            .set_recv_buffer_size(1_024)
            .expect("constrain target receive buffer");
        let _ = release_rx.recv();
        let mut observed = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut observed).expect("read forwarded prefix");
        observed_tx
            .send(observed.len())
            .expect("send forwarded prefix size");
    });
    (port, release_tx, observed_rx, handle)
}

fn start_tls_proxy(hostname: &str, port: u16, ech: EchPolicy) -> (Proxy, Lease) {
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO),
        Arc::new(StaticResolver(IpAddr::V4(Ipv4Addr::LOCALHOST))),
    )
    .expect("start TLS test proxy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            tls_hostname_policy(hostname, port, ech),
        )
        .expect("attach TLS lease");
    (proxy, lease)
}

fn open_tls_tunnel(lease: &Lease, hostname: &str, port: u16) -> std::net::TcpStream {
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set client timeout");
    std::io::Write::write_all(
        &mut client,
        format!("CONNECT {hostname}:{port} HTTP/1.1\r\n\r\n").as_bytes(),
    )
    .expect("write CONNECT");
    let mut response = [0; 39];
    std::io::Read::read_exact(&mut client, &mut response).expect("read CONNECT response");
    assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    client
}

fn assert_tunnel_closed(client: &mut std::net::TcpStream) {
    let mut byte = [0];
    match std::io::Read::read(client, &mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
            ) => {}
        result => panic!("denied tunnel remained readable: {result:?}"),
    }
}

#[test]
fn matching_tls_sni_forwards_the_exact_client_hello() {
    let hello = client_hello(Some("allowed.test"), false);
    let (port, captured_rx, target) = start_tls_capture(hello.len());
    let (proxy, lease) = start_tls_proxy("allowed.test", port, EchPolicy::Reject);
    let mut client =
        std::net::TcpStream::connect(lease.endpoint().socket_addr()).expect("connect proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set client timeout");
    let mut request = format!("CONNECT allowed.test:{port} HTTP/1.1\r\n\r\n").into_bytes();
    request.extend_from_slice(&hello);
    std::io::Write::write_all(&mut client, &request).expect("write coalesced CONNECT and hello");
    let mut response_and_marker = [0; 40];
    std::io::Read::read_exact(&mut client, &mut response_and_marker)
        .expect("read CONNECT response and marker");
    assert_eq!(
        &response_and_marker[..39],
        b"HTTP/1.1 200 Connection Established\r\n\r\n"
    );
    assert_eq!(response_and_marker[39], b'x');
    assert_eq!(
        captured_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("captured ClientHello"),
        hello
    );
    target.join().expect("TLS capture target");
    drop(client);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close inspected lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, hello.len() as u64);
    assert_eq!(final_usage.downloaded_bytes, 1);
    assert_eq!(final_usage.denied_connections, 0);
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn mismatched_tls_sni_is_never_forwarded_upstream() {
    let hello = client_hello(Some("other.test"), false);
    let (port, observed_rx, target) = start_tls_denial_observer();
    let (proxy, lease) = start_tls_proxy("allowed.test", port, EchPolicy::Reject);
    let mut client = open_tls_tunnel(&lease, "allowed.test", port);
    std::io::Write::write_all(&mut client, &hello).expect("write mismatched ClientHello");
    assert_tunnel_closed(&mut client);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observed denied target"),
        0
    );
    target.join().expect("TLS denial target");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close denied lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, hello.len() as u64);
    assert_eq!(final_usage.downloaded_bytes, 0);
    assert_eq!(final_usage.denied_connections, 1);
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn ech_requires_an_explicit_outer_sni_compatibility_policy() {
    let hello = client_hello(Some("allowed.test"), true);
    let (strict_port, observed_rx, strict_target) = start_tls_denial_observer();
    let (strict_proxy, strict_lease) =
        start_tls_proxy("allowed.test", strict_port, EchPolicy::Reject);
    let mut strict_client = open_tls_tunnel(&strict_lease, "allowed.test", strict_port);
    std::io::Write::write_all(&mut strict_client, &hello).expect("write ECH ClientHello");
    assert_tunnel_closed(&mut strict_client);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observed strict ECH target"),
        0
    );
    strict_target.join().expect("strict ECH target");
    assert_eq!(
        strict_lease
            .close(Instant::now() + Duration::from_secs(1))
            .expect("close strict ECH lease")
            .usage()
            .denied_connections,
        1
    );
    strict_proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("strict proxy shutdown");

    let (compat_port, captured_rx, compat_target) = start_tls_capture(hello.len());
    let (compat_proxy, compat_lease) =
        start_tls_proxy("allowed.test", compat_port, EchPolicy::AllowOuterSni);
    let mut compat_client = open_tls_tunnel(&compat_lease, "allowed.test", compat_port);
    std::io::Write::write_all(&mut compat_client, &hello).expect("write compatible ECH hello");
    let mut marker = [0];
    std::io::Read::read_exact(&mut compat_client, &mut marker).expect("read target marker");
    assert_eq!(marker[0], b'x');
    assert_eq!(
        captured_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("captured compatible ECH hello"),
        hello
    );
    compat_target.join().expect("compatible ECH target");
    drop(compat_client);
    let final_usage = compat_lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close compatible ECH lease")
        .usage();
    assert_eq!(final_usage.denied_connections, 0);
    assert_eq!(final_usage.uploaded_bytes, hello.len() as u64);
    compat_proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("compatible proxy shutdown");
}

#[test]
fn close_cancels_a_partial_client_hello_without_forwarding_it() {
    let hello = client_hello(Some("allowed.test"), false);
    let partial = &hello[..11];
    let (port, observed_rx, target) = start_tls_denial_observer();
    let (proxy, lease) = start_tls_proxy("allowed.test", port, EchPolicy::Reject);
    let mut client = open_tls_tunnel(&lease, "allowed.test", port);
    std::io::Write::write_all(&mut client, partial).expect("write partial ClientHello");
    let observed_deadline = Instant::now() + Duration::from_secs(1);
    while lease.usage().uploaded_bytes != partial.len() as u64 && Instant::now() < observed_deadline
    {
        thread::yield_now();
    }
    assert_eq!(lease.usage().uploaded_bytes, partial.len() as u64);

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close partial ClientHello lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, partial.len() as u64);
    assert_eq!(final_usage.active_connections, 0);
    assert_eq!(final_usage.denied_connections, 0);
    assert_tunnel_closed(&mut client);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe revoked upstream"),
        0
    );
    target.join().expect("revoked TLS target");
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn absolute_handshake_deadline_cancels_a_partial_client_hello() {
    let hello = client_hello(Some("allowed.test"), false);
    let partial = &hello[..11];
    let (port, observed_rx, target) = start_tls_denial_observer();
    let proxy = Proxy::start_with_test_resolver(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO),
        Arc::new(StaticResolver(IpAddr::V4(Ipv4Addr::LOCALHOST))),
    )
    .expect("start deadline proxy");
    let policy = Policy::builder()
        .allow_host("allowed.test")
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(port)
        .require_tls_sni()
        .dns_timeout(Duration::from_millis(50))
        .handshake_timeout(Duration::from_millis(50))
        .build()
        .expect("valid deadline policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach deadline lease");
    let mut client = open_tls_tunnel(&lease, "allowed.test", port);
    std::io::Write::write_all(&mut client, partial).expect("write partial ClientHello");
    assert_tunnel_closed(&mut client);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe deadline upstream"),
        0
    );
    target.join().expect("deadline TLS target");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close deadline lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, partial.len() as u64);
    assert_eq!(final_usage.denied_connections, 1);
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}

#[test]
fn absolute_handshake_deadline_cancels_blocked_client_hello_forwarding() {
    let hello = client_hello_with_padding(Some("allowed.test"), false, 64_000);
    let hello = fragment_records(&hello, 16_384);
    assert!(
        hello.len() < 65_536,
        "fixture must fit the configured bound"
    );
    let (port, release_target, observed_rx, target) = start_nonreading_target();
    let proxy = Proxy::start_with_test_backends(
        ProxyConfig::default().with_identity_reuse_quiet_period(Duration::ZERO),
        Arc::new(StaticResolver(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        Arc::new(ConstrainedConnector {
            send_buffer_bytes: 1_024,
        }),
    )
    .expect("start constrained proxy");
    let policy = Policy::builder()
        .allow_host("allowed.test")
        .expect("valid test hostname")
        .allow_network("127.0.0.0/8".parse().expect("valid loopback test network"))
        .allow_port(port)
        .require_tls_sni()
        .dns_timeout(Duration::from_millis(250))
        .handshake_timeout(Duration::from_millis(250))
        .build()
        .expect("valid constrained policy");
    let lease = proxy
        .attach(
            PeerIdentity::SourceIp(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            policy,
        )
        .expect("attach constrained lease");
    let mut client = open_tls_tunnel(&lease, "allowed.test", port);
    std::io::Write::write_all(&mut client, &hello).expect("write large ClientHello");
    assert_tunnel_closed(&mut client);
    release_target.send(()).expect("release constrained target");
    let observed = observed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("observe forwarded prefix");
    assert!(
        observed < hello.len(),
        "the constrained upstream unexpectedly received the full ClientHello"
    );
    target.join().expect("constrained target");

    let final_usage = lease
        .close(Instant::now() + Duration::from_secs(1))
        .expect("close constrained lease")
        .usage();
    assert_eq!(final_usage.uploaded_bytes, hello.len() as u64);
    assert_eq!(final_usage.denied_connections, 1);
    assert_eq!(final_usage.active_connections, 0);
    proxy
        .shutdown(Instant::now() + Duration::from_secs(1))
        .expect("proxy shutdown");
}
